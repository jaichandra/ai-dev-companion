const test = require("node:test");
const assert = require("node:assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { isPurgeable, dataFolderUsage, purgeDataFolder, owningClone } = require("./data-folder.js");

function fixture() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "data-folder-"));
  fs.mkdirSync(path.join(dir, "worktrees", "repo", "job1"), { recursive: true });
  fs.writeFileSync(path.join(dir, "worktrees", "repo", "job1", "a.txt"), "x".repeat(1000));
  fs.mkdirSync(path.join(dir, "repo.worktrees", "pr-review"), { recursive: true });
  fs.writeFileSync(path.join(dir, "repo.worktrees", "pr-review", "b.txt"), "y".repeat(500));
  fs.mkdirSync(path.join(dir, "jobs"));
  fs.writeFileSync(path.join(dir, "jobs", "j.json"), "{}");
  fs.writeFileSync(path.join(dir, "history.db"), "z".repeat(200));
  return dir;
}

test("only checkouts are purgeable", () => {
  assert.ok(isPurgeable("worktrees"));
  assert.ok(isPurgeable("repos"));
  assert.ok(isPurgeable("sample-app.worktrees"));
  for (const keep of ["jobs", "history.db", "sessions", "credentials.key", "worktrees.json"]) {
    assert.ok(!isPurgeable(keep), keep);
  }
});

test("usage totals the folder and splits out what a purge frees", async () => {
  const dir = fixture();
  const u = await dataFolderUsage(dir);
  assert.ok(u.totalBytes >= 1700);
  assert.ok(u.purgeableBytes >= 1500 && u.purgeableBytes < u.totalBytes);
  assert.deepStrictEqual(u.items.filter((i) => i.purgeable).map((i) => i.name).sort(), ["repo.worktrees", "worktrees"]);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a missing folder reports zero", async () => {
  const u = await dataFolderUsage(path.join(os.tmpdir(), "does-not-exist-data-folder"));
  assert.strictEqual(u.totalBytes, 0);
});

test("purge removes checkouts and keeps history, jobs and the rest", async () => {
  const dir = fixture();
  const r = await purgeDataFolder(dir);
  assert.deepStrictEqual(r.removed.sort(), ["repo.worktrees", "worktrees"]);
  assert.ok(r.freedBytes >= 1500);
  assert.deepStrictEqual(fs.readdirSync(dir).sort(), ["history.db", "jobs"]);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a symlink inside a worktree is not followed", async () => {
  const dir = fixture();
  const outside = fs.mkdtempSync(path.join(os.tmpdir(), "outside-"));
  fs.writeFileSync(path.join(outside, "keep.txt"), "k".repeat(5000));
  fs.symlinkSync(outside, path.join(dir, "worktrees", "repo", "job1", "node_modules"), "dir");
  const u = await dataFolderUsage(dir);
  assert.ok(u.totalBytes < 5000);
  await purgeDataFolder(dir);
  assert.ok(fs.existsSync(path.join(outside, "keep.txt")));
  fs.rmSync(dir, { recursive: true, force: true });
  fs.rmSync(outside, { recursive: true, force: true });
});

test("owningClone reads the clone from a worktree's .git file", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "data-folder-"));
  const g = path.join("/x", "clone", ".git", "worktrees", "job1");
  fs.writeFileSync(path.join(dir, ".git"), `gitdir: ${g}\n`);
  assert.strictEqual(owningClone(dir), path.join("/x", "clone"));
  assert.strictEqual(owningClone(path.join(dir, "nope")), null);
  fs.rmSync(dir, { recursive: true, force: true });
});
