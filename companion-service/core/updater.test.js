const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const updater = require("./updater.js");

const REPO_ROOT = path.join(__dirname, "..", "..");

function versionOf(relativePath) {
  return JSON.parse(fs.readFileSync(path.join(REPO_ROOT, relativePath), "utf8")).version;
}


test("CHANGELOG.md has What's New notes for the current version", () => {
  const entry = updater.readChangelog(REPO_ROOT).find((e) => e.version === versionOf("package.json"));
  assert.ok(entry, `CHANGELOG.md needs a "## ${versionOf("package.json")}" section`);
  assert.ok(entry.notes.length > 0);
});

test("parseChangelog reads release sections and their bullets as plain text", () => {
  const text = [
    "# Changelog",
    "",
    "- intro bullet, not in a release",
    "",
    "## Unreleased",
    "- not released yet",
    "",
    "## 0.5.0 — 2026-10-01",
    "- **What's New** in the `update` panel.",
    "- A long note that",
    "  wraps onto a second line.",
    "",
    "Loose text is skipped.",
    "",
    "## v0.4.2",
    "* See [the docs](docs/DESIGN.md).",
    "",
    "## 0.4.1",
  ].join("\n");
  assert.deepEqual(updater.parseChangelog(text), [
    {
      version: "0.5.0",
      notes: ["What's New in the update panel.", "A long note that wraps onto a second line."],
    },
    { version: "0.4.2", notes: ["See the docs."] },
  ]);
  assert.deepEqual(updater.parseChangelog(""), []);
});

test("whatsNewSince keeps the versions after the installed one, up to the latest, newest first", () => {
  const entries = ["0.4.0", "0.6.0", "0.4.2", "0.5.0", "0.4.1"].map((version) => ({
    version,
    notes: [version],
  }));
  assert.deepEqual(
    updater.whatsNewSince(entries, "0.4.1", "0.5.0").map((e) => e.version),
    ["0.5.0", "0.4.2"],
  );
  assert.deepEqual(
    updater.whatsNewSince(entries, "0.4.1", null).map((e) => e.version),
    ["0.6.0", "0.5.0", "0.4.2"],
  );
  assert.deepEqual(updater.whatsNewSince(entries, "0.6.0", "0.6.0"), []);
});

test("compareVersions orders numerically, not as strings", () => {
  assert.equal(updater.compareVersions("0.10.0", "0.9.9"), 1);
  assert.equal(updater.compareVersions("0.4.0", "0.4"), 0);
  assert.equal(updater.compareVersions("v1.2.3", "1.2.4"), -1);
  assert.equal(updater.compareVersions("1.2.3-beta", "1.2.3"), 0);
  assert.equal(updater.compareVersions(null, "0.0.1"), -1);
});

test("parseInstallSource falls back to the default repo and rejects odd refs", () => {
  assert.deepEqual(updater.parseInstallSource(null), updater.DEFAULT_SOURCE);
  assert.deepEqual(updater.parseInstallSource({ url: " /tmp/repo ", ref: "release/1.x" }), {
    url: "/tmp/repo",
    ref: "release/1.x",
  });
  assert.equal(updater.parseInstallSource({ url: "/tmp/repo", ref: "--upload-pack=x y" }).ref, updater.DEFAULT_SOURCE.ref);
});

test("resolveInstallSource prefers package.json's updateSource, then the git origin, then the default", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "updater-src-"));
  const writePkg = (pkg) => fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(pkg));
  const origin = "https://bitbucket.example.com/scm/~me/fork.git";

  writePkg({ updateSource: { url: "https://bitbucket.example.com/scm/team/moved.git" } });
  assert.deepEqual(updater.resolveInstallSource({ sourceDir: dir, origin }), {
    url: "https://bitbucket.example.com/scm/team/moved.git",
    ref: updater.DEFAULT_SOURCE.ref,
  });

  writePkg({ name: "no-update-source" });
  assert.deepEqual(updater.resolveInstallSource({ sourceDir: dir, origin }), {
    url: origin,
    ref: updater.DEFAULT_SOURCE.ref,
  });
  assert.deepEqual(updater.resolveInstallSource({ sourceDir: dir }), updater.DEFAULT_SOURCE);

  writePkg({ updateSource: { url: "https://bitbucket.example.com/scm/team/moved.git", ref: "release" } });
  assert.deepEqual(updater.resolveInstallSource({ sourceDir: dir, url: "/tmp/test-repo", ref: "test" }), {
    url: "/tmp/test-repo",
    ref: "test",
  });

  fs.rmSync(dir, { recursive: true, force: true });
});


test("manualUpdateCommand gives the npx command for a remote and git pull for a local checkout", () => {
  assert.equal(
    updater.manualUpdateCommand(updater.DEFAULT_SOURCE),
    `npx --yes "git+${updater.DEFAULT_SOURCE.url}#${updater.DEFAULT_SOURCE.ref}"`,
  );
  assert.equal(
    updater.manualUpdateCommand({ url: "git@host:team/repo.git", ref: "dev" }),
    'npx --yes "git+ssh://git@host/team/repo.git#dev"',
  );
  assert.equal(
    updater.manualUpdateCommand({ url: "/Users/me/src/repo", ref: "master" }),
    'cd "/Users/me/src/repo" && git pull && node install.js',
  );
});

test("syncSource clones the source, then picks up new commits", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "updater-test-"));
  const origin = path.join(root, "origin");
  const git = (...args) => execFileSync("git", args, { cwd: origin, stdio: "ignore" });
  fs.mkdirSync(origin);
  git("init", "-q", "-b", "master");
  git("config", "user.email", "t@example.com");
  git("config", "user.name", "t");
  fs.writeFileSync(path.join(origin, "package.json"), '{"version":"1.0.0"}');
  git("add", ".");
  git("commit", "-qm", "v1");

  const source = { url: origin, ref: "master" };
  const dir = path.join(root, "cache");
  await updater.syncSource(source, dir);
  assert.equal(updater.installedVersion(dir), "1.0.0");

  fs.writeFileSync(path.join(origin, "package.json"), '{"version":"1.1.0"}');
  git("commit", "-qam", "v1.1");
  await updater.syncSource(source, dir);
  assert.equal(updater.installedVersion(dir), "1.1.0");

  fs.rmSync(root, { recursive: true, force: true });
});

test("readStatus reports a 'running' update whose runner is gone as failed", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "updater-status-"));
  const file = path.join(dir, "status.json");
  updater.writeStatus({ state: "running", pid: 999999, to: "9.9.9" }, file);
  assert.equal(updater.readStatus(file).state, "failed");
  updater.writeStatus({ state: "running", pid: process.pid }, file);
  assert.equal(updater.readStatus(file).state, "running");
  updater.writeStatus({ state: "done", pid: 999999 }, file);
  assert.equal(updater.readStatus(file).state, "done");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("statusPath and logPath prefer the new state dir's file when it already exists there", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "updater-paths-"));
  const newDir = path.join(home, ".ai-dev-companion");
  fs.mkdirSync(newDir, { recursive: true });
  fs.writeFileSync(path.join(newDir, "update-status.json"), "{}");
  fs.writeFileSync(path.join(newDir, "update.log"), "");

  assert.equal(updater.statusPath(home), path.join(newDir, "update-status.json"));
  assert.equal(updater.logPath(home), path.join(newDir, "update.log"));
  fs.rmSync(home, { recursive: true, force: true });
});

test("statusPath and logPath fall back to the legacy dir's file when only that one exists", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "updater-paths-"));
  const legacyDir = path.join(home, ".bitbucket-ai-companion");
  fs.mkdirSync(legacyDir, { recursive: true });
  fs.writeFileSync(path.join(legacyDir, "update-status.json"), "{}");
  fs.writeFileSync(path.join(legacyDir, "update.log"), "");

  assert.equal(updater.statusPath(home), path.join(legacyDir, "update-status.json"));
  assert.equal(updater.logPath(home), path.join(legacyDir, "update.log"));
  fs.rmSync(home, { recursive: true, force: true });
});

test("statusPath and logPath default to the new state dir when neither file exists yet", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "updater-paths-"));

  assert.equal(updater.statusPath(home), path.join(home, ".ai-dev-companion", "update-status.json"));
  assert.equal(updater.logPath(home), path.join(home, ".ai-dev-companion", "update.log"));
  fs.rmSync(home, { recursive: true, force: true });
});

test("newStatusPath and newLogPath always point at the new state dir, even when only the legacy dir has the file (writes never revive the legacy dir)", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "updater-paths-"));
  const legacyDir = path.join(home, ".bitbucket-ai-companion");
  fs.mkdirSync(legacyDir, { recursive: true });
  fs.writeFileSync(path.join(legacyDir, "update-status.json"), "{}");
  fs.writeFileSync(path.join(legacyDir, "update.log"), "");

  assert.equal(updater.newStatusPath(home), path.join(home, ".ai-dev-companion", "update-status.json"));
  assert.equal(updater.newLogPath(home), path.join(home, ".ai-dev-companion", "update.log"));
  fs.rmSync(home, { recursive: true, force: true });
});

test("writeStatus's default file is newStatusPath, not the sticky legacy-fallback statusPath", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "updater-write-"));
  const legacyDir = path.join(home, ".bitbucket-ai-companion");
  fs.mkdirSync(legacyDir, { recursive: true });
  fs.writeFileSync(path.join(legacyDir, "update-status.json"), "{}");
  const originalHomedir = os.homedir;
  os.homedir = () => home;
  try {
    updater.writeStatus({ state: "done" });
    assert.ok(fs.existsSync(path.join(home, ".ai-dev-companion", "update-status.json")));
    assert.equal(fs.readFileSync(path.join(legacyDir, "update-status.json"), "utf8"), "{}");
  } finally {
    os.homedir = originalHomedir;
  }
  fs.rmSync(home, { recursive: true, force: true });
});
