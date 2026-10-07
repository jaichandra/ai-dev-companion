const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const { openLocation, OpenLocationError } = require("../dist/core/open-location.js");

function makeRepo(files) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "pai-open-")));
  execFileSync("git", ["init", "-q"], { cwd: root });
  for (const [rel, content] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(root, rel)), { recursive: true });
    if (content && content.symlink) fs.symlinkSync(content.symlink, path.join(root, rel));
    else fs.writeFileSync(path.join(root, rel), content);
  }
  execFileSync("git", ["add", "-A"], { cwd: root });
  return root;
}

const deps = (opened) => ({ open: async (u) => void opened.push(u), installedEditors: () => ["vscode"] });

test("opens a tracked file inside the repo", async () => {
  const root = makeRepo({ "src/a.ts": "x" });
  const opened = [];
  const r = await openLocation({ repos: { "P/r": root } }, { text: "src/a.ts:3" }, deps(opened));
  assert.equal(r.path, "src/a.ts");
  assert.equal(opened.length, 1);
});

test("a tracked symlink that points outside the repo is treated as not found", async () => {
  const outside = fs.mkdtempSync(path.join(os.tmpdir(), "pai-outside-"));
  fs.writeFileSync(path.join(outside, "secret.ts"), "x");
  const root = makeRepo({ "src/link.ts": { symlink: path.join(outside, "secret.ts") } });
  const opened = [];
  await assert.rejects(
    openLocation({ repos: { "P/r": root } }, { text: "src/link.ts:1" }, deps(opened)),
    (e) => e instanceof OpenLocationError && e.code === "not-found",
  );
  assert.equal(opened.length, 0);
});

test("a failing git ls-files surfaces as git-failed and is not cached", async () => {
  const root = makeRepo({ "src/a.ts": "x" });
  const gitDir = path.join(root, ".git");
  const moved = `${gitDir}.moved`;
  // Leave a .git entry that exists but is not a repository.
  fs.renameSync(gitDir, moved);
  fs.mkdirSync(gitDir);
  const opened = [];
  await assert.rejects(
    openLocation({ repos: { "P/r": root } }, { text: "src/a.ts:1" }, deps(opened)),
    (e) => e instanceof OpenLocationError && e.code === "git-failed",
  );
  // Repair: the next call must retry rather than reuse a cached empty list.
  fs.rmSync(gitDir, { recursive: true, force: true });
  fs.renameSync(moved, gitDir);
  const r = await openLocation({ repos: { "P/r": root } }, { text: "src/a.ts:1" }, deps(opened));
  assert.equal(r.path, "src/a.ts");
});
