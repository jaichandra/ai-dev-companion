const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { snapshotWorktreeGitMetadata, worktreeGitMetadataChanged } = require("./worktree-integrity.js");

/** A fresh temp dir laid out like a real worktree's private metadata:
 * `<tmp>/worktree/.git` (the gitlink file) and `<tmp>/gitdir/{commondir,gitdir}`
 * (config.worktree deliberately absent — most worktrees never have one). */
function makeFixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "worktree-integrity-test-"));
  const worktreeDir = path.join(root, "worktree");
  const gitDir = path.join(root, "gitdir");
  fs.mkdirSync(worktreeDir, { recursive: true });
  fs.mkdirSync(gitDir, { recursive: true });
  fs.writeFileSync(path.join(worktreeDir, ".git"), `gitdir: ${gitDir}\n`);
  fs.writeFileSync(path.join(gitDir, "commondir"), "../..\n");
  fs.writeFileSync(path.join(gitDir, "gitdir"), `${worktreeDir}/.git\n`);
  return { root, worktreeDir, gitDir };
}

test("snapshotWorktreeGitMetadata reads the gitlink, commondir, gitdir, and null for a missing config.worktree", () => {
  const { worktreeDir, gitDir } = makeFixture();
  const snap = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  assert.equal(snap.gitlink, `gitdir: ${gitDir}\n`);
  assert.equal(snap.commondir, "../..\n");
  assert.equal(snap.gitdir, `${worktreeDir}/.git\n`);
  assert.equal(snap.configWorktree, null);
});

test("worktreeGitMetadataChanged is false for two snapshots of untouched metadata", () => {
  const { worktreeDir, gitDir } = makeFixture();
  const before = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  const after = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  assert.equal(worktreeGitMetadataChanged(before, after), false);
});

test("worktreeGitMetadataChanged is true when the gitlink file is rewritten", () => {
  const { worktreeDir, gitDir } = makeFixture();
  const before = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  fs.writeFileSync(path.join(worktreeDir, ".git"), "gitdir: /somewhere/else\n");
  const after = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  assert.equal(worktreeGitMetadataChanged(before, after), true);
});

test("worktreeGitMetadataChanged is true when commondir is rewritten", () => {
  const { worktreeDir, gitDir } = makeFixture();
  const before = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  fs.writeFileSync(path.join(gitDir, "commondir"), "/attacker/controlled\n");
  const after = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  assert.equal(worktreeGitMetadataChanged(before, after), true);
});

test("worktreeGitMetadataChanged is true when config.worktree appears (was absent before)", () => {
  const { worktreeDir, gitDir } = makeFixture();
  const before = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  fs.writeFileSync(path.join(gitDir, "config.worktree"), "[core]\n\thooksPath = /tmp/evil\n");
  const after = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  assert.equal(worktreeGitMetadataChanged(before, after), true);
});

test("worktreeGitMetadataChanged is true when gitdir is rewritten", () => {
  const { worktreeDir, gitDir } = makeFixture();
  const before = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  fs.writeFileSync(path.join(gitDir, "gitdir"), "/attacker/controlled/.git\n");
  const after = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
  assert.equal(worktreeGitMetadataChanged(before, after), true);
});
