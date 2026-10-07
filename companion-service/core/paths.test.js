const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const paths = require("./paths.js");

/** A fresh temp `home` for one test, cleaned up by the caller. */
function tempHome() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "paths-test-"));
}

/** A fake `.git` *file* worktree checkout at
 * `<home>/<dirName>/worktrees/<repo>/<id>/`, matching what
 * core/worktree.ts's addWorktree creates and what migrateStateDir looks
 * for (a worktree's `.git` is a one-line file, unlike a real clone's). */
function makeFakeWorktree(home, dirName, repo, id) {
  const dir = path.join(home, dirName, "worktrees", repo, id);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, ".git"), "gitdir: /somewhere/else\n");
  return dir;
}

test("stateDir and legacyStateDir join the state dir name onto home", () => {
  assert.equal(paths.stateDir("/h"), path.join("/h", ".ai-dev-companion"));
  assert.equal(paths.legacyStateDir("/h"), path.join("/h", ".bitbucket-ai-companion"));
  assert.equal(paths.STATE_DIR_NAME, ".ai-dev-companion");
  assert.equal(paths.LEGACY_STATE_DIR_NAME, ".bitbucket-ai-companion");
});

test("migrateStateDir moves the legacy dir to the new one and reports migrated: true", () => {
  const home = tempHome();
  const legacy = paths.legacyStateDir(home);
  const current = paths.stateDir(home);
  fs.mkdirSync(legacy, { recursive: true });
  fs.writeFileSync(path.join(legacy, "config.json"), "{}");

  const result = paths.migrateStateDir({ home, git: () => {} });

  assert.equal(result.migrated, true);
  assert.deepEqual(result.errors, []);
  assert.ok(fs.existsSync(path.join(current, "config.json")));
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir no-ops when only the new dir already exists", () => {
  const home = tempHome();
  fs.mkdirSync(paths.stateDir(home), { recursive: true });

  const result = paths.migrateStateDir({ home, git: () => {} });

  assert.deepEqual(result, { migrated: false, repaired: [], errors: [] });
  assert.ok(!fs.existsSync(paths.legacyStateDir(home)));
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir no-ops when neither dir exists", () => {
  const home = tempHome();

  const result = paths.migrateStateDir({ home, git: () => {} });

  assert.deepEqual(result, { migrated: false, repaired: [], errors: [] });
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir does not overwrite the new dir when both already exist", () => {
  const home = tempHome();
  fs.mkdirSync(paths.legacyStateDir(home), { recursive: true });
  fs.writeFileSync(path.join(paths.legacyStateDir(home), "legacy-marker"), "");
  fs.mkdirSync(paths.stateDir(home), { recursive: true });
  fs.writeFileSync(path.join(paths.stateDir(home), "new-marker"), "");

  const result = paths.migrateStateDir({ home, git: () => {} });

  assert.equal(result.migrated, false);
  assert.ok(fs.existsSync(path.join(paths.legacyStateDir(home), "legacy-marker")));
  assert.ok(fs.existsSync(path.join(paths.stateDir(home), "new-marker")));
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir repairs every moved worktree with the injected git runner", () => {
  const home = tempHome();
  makeFakeWorktree(home, ".bitbucket-ai-companion", "some-repo", "job-1");
  const calls = [];
  const git = (args, cwd) => {
    calls.push({ args, cwd });
  };

  const result = paths.migrateStateDir({ home, git });

  const newWorktreePath = path.join(paths.stateDir(home), "worktrees", "some-repo", "job-1");
  assert.deepEqual(calls, [{ args: ["worktree", "repair", newWorktreePath], cwd: newWorktreePath }]);
  assert.deepEqual(result.repaired, [newWorktreePath]);
  assert.deepEqual(result.errors, []);
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir writes a MOVED marker pointing at the new location", () => {
  const home = tempHome();
  fs.mkdirSync(paths.legacyStateDir(home), { recursive: true });

  paths.migrateStateDir({ home, git: () => {} });

  const movedFile = path.join(paths.legacyStateDir(home), "MOVED");
  assert.deepEqual(fs.readdirSync(paths.legacyStateDir(home)), ["MOVED"]);
  assert.equal(fs.readFileSync(movedFile, "utf8").trim(), paths.stateDir(home));
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir never throws when the injected git runner fails, and records it in errors", () => {
  const home = tempHome();
  makeFakeWorktree(home, ".bitbucket-ai-companion", "some-repo", "job-1");
  const logged = [];
  const git = () => {
    throw new Error("git not found");
  };

  const result = paths.migrateStateDir({ home, git, log: (msg) => logged.push(msg) });

  assert.equal(result.migrated, true);
  assert.deepEqual(result.repaired, []);
  assert.equal(result.errors.length, 1);
  assert.match(result.errors[0], /git not found/);
  assert.equal(logged.length, 1);
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir skips a dangling symlink under worktrees/ instead of throwing, and still repairs the real ones", () => {
  const home = tempHome();
  makeFakeWorktree(home, ".bitbucket-ai-companion", "some-repo", "job-1");
  const worktreesRoot = path.join(home, ".bitbucket-ai-companion", "worktrees");
  // A whole repo entry that's a dangling symlink (nothing at its target).
  fs.symlinkSync(path.join(worktreesRoot, "does-not-exist"), path.join(worktreesRoot, "broken-repo-link"));
  // A dangling symlink for one id entry inside a real repo dir.
  fs.symlinkSync(
    path.join(worktreesRoot, "some-repo", "does-not-exist"),
    path.join(worktreesRoot, "some-repo", "broken-id-link"),
  );
  const calls = [];
  const git = (args, cwd) => calls.push({ args, cwd });

  const result = paths.migrateStateDir({ home, git });

  const newWorktreePath = path.join(paths.stateDir(home), "worktrees", "some-repo", "job-1");
  assert.deepEqual(result.repaired, [newWorktreePath]);
  assert.deepEqual(result.errors, []);
  assert.deepEqual(calls, [{ args: ["worktree", "repair", newWorktreePath], cwd: newWorktreePath }]);
  fs.rmSync(home, { recursive: true, force: true });
});

test("migrateStateDir prefers err.stderr over err.message when the git runner sets it", () => {
  const home = tempHome();
  makeFakeWorktree(home, ".bitbucket-ai-companion", "some-repo", "job-1");
  const git = () => {
    const err = new Error("Command failed: git worktree repair");
    err.stderr = "fatal: not a git repository\n";
    throw err;
  };

  const result = paths.migrateStateDir({ home, git });

  assert.equal(result.errors.length, 1);
  assert.match(result.errors[0], /fatal: not a git repository/);
  assert.doesNotMatch(result.errors[0], /Command failed/);
  fs.rmSync(home, { recursive: true, force: true });
});

test("shouldMigrate runs only for the stable install, and never when the operator opted out via env", () => {
  assert.equal(paths.shouldMigrate({ isStableInstall: true, env: {} }), true);
  assert.equal(paths.shouldMigrate({ isStableInstall: false, env: {} }), false);
  assert.equal(
    paths.shouldMigrate({ isStableInstall: true, env: { AI_DEV_COMPANION_SKIP_MIGRATION: "1" } }),
    false,
  );
  assert.equal(
    paths.shouldMigrate({ isStableInstall: true, env: { AI_DEV_COMPANION_SKIP_MIGRATION: "0" } }),
    true,
  );
});
