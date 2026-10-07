// scripts/assemble.js: how a framework and a distribution become the tree that runs.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { assemble, FRAMEWORK_DIRS } = require("./assemble.js");

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "assemble-test-"));
function write(root, rel, text = rel) {
  const file = path.join(root, rel);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, text);
}
const read = (root, rel) => fs.readFileSync(path.join(root, rel), "utf8");

function framework() {
  const dir = tmp();
  write(dir, "companion-service/environment.js", "placeholder");
  write(dir, "companion-service/core/a.js", "a");
  write(dir, "companion-service/package.json", JSON.stringify({ name: "svc", version: "0.1.0" }));
  write(dir, "chrome-extension/registry.js", "registry");
  write(dir, "chrome-extension/manifest.json", JSON.stringify({ name: "x", version: "0.1.0" }));
  write(dir, "plugin/hooks/hooks.json", "{}");
  write(dir, "README.md", "the framework's own readme");
  write(dir, "scripts/assemble.js", "tooling");
  write(dir, "package.json", "framework root");
  return dir;
}
function distribution() {
  const dir = tmp();
  write(dir, "package.json", "distribution root");
  write(dir, "install.js", "installer");
  write(dir, "dist-files/companion-service/environment.js", "distribution profile");
  write(dir, "dist-files/companion-service/packs/mine/index.js", "pack");
  write(dir, "dist-files/plugin/.claude-plugin/plugin.json", JSON.stringify({ name: "p", version: "9.9.9" }));
  return dir;
}

test("the framework's three folders, the distribution's root files and its overlay merge into one tree", () => {
  const dest = path.join(tmp(), "out");
  const dist = distribution();
  assemble({ frameworkDir: framework(), rootDir: dist, rootSkip: ["dist-files"], overlays: [path.join(dist, "dist-files")], dest });
  assert.equal(read(dest, "companion-service/environment.js"), "distribution profile", "the overlay replaces the placeholder");
  assert.equal(read(dest, "companion-service/core/a.js"), "a");
  assert.equal(read(dest, "companion-service/packs/mine/index.js"), "pack");
  assert.equal(read(dest, "chrome-extension/registry.js"), "registry");
  assert.equal(read(dest, "package.json"), "distribution root", "the distribution's root files, not the framework's");
  assert.equal(read(dest, "install.js"), "installer");
  assert.ok(!fs.existsSync(path.join(dest, "scripts")) && !fs.existsSync(path.join(dest, "dist-files")));
  assert.ok(!fs.existsSync(path.join(dest, "README.md")), "the framework's own readme is not copied");
});

test("without a distribution, only the framework's folders are assembled", () => {
  const dest = path.join(tmp(), "out");
  assemble({ frameworkDir: framework(), dest });
  assert.deepEqual(fs.readdirSync(dest).sort(), [...FRAMEWORK_DIRS].sort());
  assert.equal(read(dest, "companion-service/environment.js"), "placeholder");
});

test("this machine's state and build output never travel", () => {
  const fw = framework();
  for (const rel of [
    "companion-service/config.json",
    "companion-service/credentials.enc",
    "companion-service/analysis-cache/x.json",
    "companion-service/summary-cache/x.json",
    "companion-service/install-source.json",
    "companion-service/companion-service.out.log",
    "companion-service/node_modules/dep/index.js",
    "companion-service/dist/server.js",
    "chrome-extension/companion-config.js",
    "companion-service/.DS_Store",
  ]) {
    write(fw, rel);
  }
  const root = tmp();
  write(root, ".claude/worktrees/old/file.js");
  write(root, ".claude/settings.local.json");
  const dest = path.join(tmp(), "out");
  assemble({ frameworkDir: fw, rootDir: root, dest });
  for (const rel of [
    "companion-service/config.json",
    "companion-service/credentials.enc",
    "companion-service/analysis-cache",
    "companion-service/summary-cache",
    "companion-service/install-source.json",
    "companion-service/companion-service.out.log",
    "companion-service/node_modules",
    "companion-service/dist",
    "chrome-extension/companion-config.js",
    "companion-service/.DS_Store",
    ".claude/worktrees",
  ]) {
    assert.ok(!fs.existsSync(path.join(dest, rel)), `${rel} was copied`);
  }
  assert.equal(read(dest, "companion-service/core/a.js"), "a");
  assert.equal(read(dest, ".claude/settings.local.json"), ".claude/settings.local.json");
});

test("notInstalled leaves matching paths out", () => {
  const root = tmp();
  write(root, "tests.json");
  write(root, "install.js");
  const dest = path.join(tmp(), "out");
  assemble({ frameworkDir: framework(), rootDir: root, dest, notInstalled: [/^tests\.json$/] });
  assert.ok(!fs.existsSync(path.join(dest, "tests.json")));
  assert.ok(fs.existsSync(path.join(dest, "install.js")));
});

test("an existing destination is replaced, not merged into", () => {
  const dest = tmp();
  write(dest, "leftover.txt");
  assemble({ frameworkDir: framework(), dest });
  assert.ok(!fs.existsSync(path.join(dest, "leftover.txt")));
});

test("the distribution's version is stamped on the service, the extension and the plugin", () => {
  const dest = path.join(tmp(), "out");
  const dist = distribution();
  assemble({ frameworkDir: framework(), rootDir: dist, rootSkip: ["dist-files"], overlays: [path.join(dist, "dist-files")], dest, version: "3.2.1" });
  for (const rel of ["companion-service/package.json", "chrome-extension/manifest.json", "plugin/.claude-plugin/plugin.json"]) {
    assert.equal(JSON.parse(read(dest, rel)).version, "3.2.1", rel);
  }
  assert.equal(JSON.parse(read(dest, "companion-service/package.json")).name, "svc", "other fields are kept");
});

test("this repo, assembled alone: the placeholder profile, the builtin features, a manifest for example.com", () => {
  const dest = path.join(tmp(), "out");
  assemble({ frameworkDir: path.join(__dirname, ".."), dest, version: "7.0.0" });
  assert.match(read(dest, "companion-service/environment.js"), /example\.com/);
  const manifest = JSON.parse(read(dest, "chrome-extension/manifest.json"));
  assert.equal(manifest.version, "7.0.0");
  assert.ok(manifest.host_permissions.includes("https://jira.example.com/*"));
  assert.ok(manifest.content_scripts[0].js.includes("features/resolve-conflict.js"));
});
