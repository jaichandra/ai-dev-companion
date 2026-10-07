const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { createBaselineStore } = require("./integrity-baselines.js");
const { baselineFile } = require("./job-files.js");

const ID = "11111111-1111-1111-1111-111111111111";
const ID_2 = "22222222-2222-2222-2222-222222222222";

/** A fresh temp root, laid out like a real worktree's private metadata:
 * `<root>/wt/.git` (the gitlink file) and `<root>/gd/{commondir,gitdir}`
 * (config.worktree deliberately absent). Mirrors
 * core/worktree-integrity.test.js's fixture. */
function makeFixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "integrity-baselines-test-"));
  const worktreeDir = path.join(root, "wt");
  const gitDir = path.join(root, "gd");
  fs.mkdirSync(worktreeDir, { recursive: true });
  fs.mkdirSync(gitDir, { recursive: true });
  fs.writeFileSync(path.join(worktreeDir, ".git"), `gitdir: ${gitDir}\n`);
  fs.writeFileSync(path.join(gitDir, "commondir"), "../..\n");
  fs.writeFileSync(path.join(gitDir, "gitdir"), `${worktreeDir}/.git\n`);
  return { root, worktreeDir, gitDir };
}

function cleanup(root) {
  fs.rmSync(root, { recursive: true, force: true });
}

test("take then check returns intact", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const store = createBaselineStore();
  store.take(ID, { worktreeDir, gitDir });
  assert.equal(store.check(ID, worktreeDir), "intact");
  cleanup(root);
});

test("rewriting the worktree's .git makes check return changed", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const store = createBaselineStore();
  store.take(ID, { worktreeDir, gitDir });
  fs.writeFileSync(path.join(worktreeDir, ".git"), "gitdir: /somewhere/else\n");
  assert.equal(store.check(ID, worktreeDir), "changed");
  cleanup(root);
});

test("creating gd/config.worktree makes check return changed", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const store = createBaselineStore();
  store.take(ID, { worktreeDir, gitDir });
  fs.writeFileSync(path.join(gitDir, "config.worktree"), "[core]\n\thooksPath = /tmp/evil\n");
  assert.equal(store.check(ID, worktreeDir), "changed");
  cleanup(root);
});

test("check on an unknown id returns missing", () => {
  const { root, worktreeDir } = makeFixture();
  const store = createBaselineStore();
  assert.equal(store.check(ID, worktreeDir), "missing");
  cleanup(root);
});

test("persistence: a fresh store on the same dir sees intact, then changed after tampering (the restart case)", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const storeA = createBaselineStore({ dir });
  storeA.take(ID, { worktreeDir, gitDir });

  const storeB = createBaselineStore({ dir });
  assert.equal(storeB.check(ID, worktreeDir), "intact");

  fs.writeFileSync(path.join(worktreeDir, ".git"), "gitdir: /somewhere/else\n");

  const storeC = createBaselineStore({ dir });
  assert.equal(storeC.check(ID, worktreeDir), "changed");
  cleanup(root);
});

test("the baseline file sits under dir, not under the worktree", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const before = fs.readdirSync(worktreeDir);
  const store = createBaselineStore({ dir });
  store.take(ID, { worktreeDir, gitDir });
  const after = fs.readdirSync(worktreeDir);
  assert.deepEqual(after, before);
  assert.equal(fs.existsSync(baselineFile(dir, ID)), true);
  cleanup(root);
});

test("the cache wins: a disk file rewritten to match a tampered worktree doesn't fool the same store", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const store = createBaselineStore({ dir });
  store.take(ID, { worktreeDir, gitDir });

  fs.writeFileSync(path.join(worktreeDir, ".git"), "gitdir: /somewhere/else\n");
  // Overwrite the on-disk baseline with one that matches the tampered
  // worktree — if check() consulted disk instead of the cache, it would
  // now (wrongly) say "intact".
  const tamperedSnapshot = {
    gitDir,
    snapshot: {
      gitlink: "gitdir: /somewhere/else\n",
      commondir: "../..\n",
      gitdir: `${worktreeDir}/.git\n`,
      configWorktree: null,
    },
  };
  fs.writeFileSync(baselineFile(dir, ID), JSON.stringify(tamperedSnapshot));

  assert.equal(store.check(ID, worktreeDir), "changed");
  cleanup(root);
});

test("truncated JSON on disk makes a fresh store return missing", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const storeA = createBaselineStore({ dir });
  storeA.take(ID, { worktreeDir, gitDir });
  fs.writeFileSync(baselineFile(dir, ID), "{ not json");

  const storeB = createBaselineStore({ dir });
  assert.equal(storeB.get(ID), null);
  assert.equal(storeB.check(ID, worktreeDir), "missing");
  cleanup(root);
});

test("a wrong shape (missing key) on disk makes a fresh store return missing", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const storeA = createBaselineStore({ dir });
  storeA.take(ID, { worktreeDir, gitDir });
  fs.writeFileSync(
    baselineFile(dir, ID),
    JSON.stringify({ gitDir, snapshot: { gitlink: "x", commondir: "y", gitdir: "z" } })
  );

  const storeB = createBaselineStore({ dir });
  assert.equal(storeB.get(ID), null);
  assert.equal(storeB.check(ID, worktreeDir), "missing");
  cleanup(root);
});

test("a wrong shape (a number value) on disk makes a fresh store return missing", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const storeA = createBaselineStore({ dir });
  storeA.take(ID, { worktreeDir, gitDir });
  fs.writeFileSync(
    baselineFile(dir, ID),
    JSON.stringify({ gitDir, snapshot: { gitlink: "x", commondir: "y", gitdir: "z", configWorktree: 1 } })
  );

  const storeB = createBaselineStore({ dir });
  assert.equal(storeB.get(ID), null);
  assert.equal(storeB.check(ID, worktreeDir), "missing");
  cleanup(root);
});

test("remove deletes the file, and a fresh store then returns missing", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const storeA = createBaselineStore({ dir });
  storeA.take(ID, { worktreeDir, gitDir });
  storeA.remove(ID);

  assert.equal(fs.existsSync(baselineFile(dir, ID)), false);
  assert.equal(storeA.get(ID), null);

  const storeB = createBaselineStore({ dir });
  assert.equal(storeB.check(ID, worktreeDir), "missing");
  cleanup(root);
});

test("with no dir set (memory only), take and check work and no file is written", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const before = fs.readdirSync(root, { recursive: true }).sort();
  const store = createBaselineStore();
  store.take(ID, { worktreeDir, gitDir });
  assert.equal(store.check(ID, worktreeDir), "intact");
  const after = fs.readdirSync(root, { recursive: true }).sort();
  assert.deepEqual(after, before);
  cleanup(root);
});

test("setDir turns on persistence for a store created without one", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "jobs");
  const store = createBaselineStore();
  store.setDir(dir);
  store.take(ID, { worktreeDir, gitDir });
  assert.equal(fs.existsSync(baselineFile(dir, ID)), true);
  cleanup(root);
});

test("take throws when the dir isn't writable (dir is a regular file)", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "not-a-dir");
  fs.writeFileSync(dir, "im a file, not a directory\n");
  const store = createBaselineStore({ dir });
  assert.throws(() => store.take(ID, { worktreeDir, gitDir }));
  cleanup(root);
});

test("a take whose disk write fails caches nothing, so a later check still returns missing", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const dir = path.join(root, "not-a-dir");
  fs.writeFileSync(dir, "im a file, not a directory\n");
  const store = createBaselineStore({ dir });
  assert.throws(() => store.take(ID, { worktreeDir, gitDir }));
  // Same store, same process: a baseline that never reached disk must not
  // be trusted from the cache either, or it would vanish on restart.
  assert.equal(store.check(ID, worktreeDir), "missing");
  cleanup(root);
});

test("a snapshot read error (gd/gitdir is a directory) makes check return changed", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const store = createBaselineStore();
  store.take(ID, { worktreeDir, gitDir });
  fs.rmSync(path.join(gitDir, "gitdir"), { force: true });
  fs.mkdirSync(path.join(gitDir, "gitdir"));
  assert.equal(store.check(ID, worktreeDir), "changed");
  cleanup(root);
});

test("check on a second, distinct job id doesn't collide with the first", () => {
  const { root, worktreeDir, gitDir } = makeFixture();
  const store = createBaselineStore();
  store.take(ID, { worktreeDir, gitDir });
  assert.equal(store.check(ID_2, worktreeDir), "missing");
  assert.equal(store.check(ID, worktreeDir), "intact");
  cleanup(root);
});

test("the process-wide singleton is exported and usable", () => {
  const { baselines } = require("./integrity-baselines.js");
  const { root, worktreeDir, gitDir } = makeFixture();
  baselines.take(ID, { worktreeDir, gitDir });
  assert.equal(baselines.check(ID, worktreeDir), "intact");
  baselines.remove(ID);
  cleanup(root);
});
