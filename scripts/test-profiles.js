#!/usr/bin/env node
// Runs the framework's whole test list against a profile that is not the placeholder (examples/bitbucket-env), the way
// a distribution runs it against its own. Catches a test that hardcodes the placeholder's GitHub site or hosts.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");
const { assemble } = require("./assemble.js");

const root = path.join(__dirname, "..");
const dest = fs.mkdtempSync(path.join(os.tmpdir(), "ai-dev-companion-profiles-"));
const tree = path.join(dest, "tree");
assemble({ frameworkDir: root, overlays: [path.join(root, "examples", "bitbucket-env")], dest: tree, version: require("../package.json").version });
// Root files that tests read (the assembled tree only holds the service, extension and plugin).
for (const file of ["CHANGELOG.md", "package.json"]) fs.copyFileSync(path.join(root, file), path.join(tree, file));
const service = path.join(tree, "companion-service");
fs.symlinkSync(path.join(root, "companion-service", "node_modules"), path.join(service, "node_modules"), "dir");

const run = (cmd, args) => {
  const result = spawnSync(cmd, args, { cwd: service, stdio: "inherit" });
  if (result.status !== 0) {
    fs.rmSync(dest, { recursive: true, force: true });
    process.exit(result.status ?? 1);
  }
};
run("npm", ["run", "build", "--silent"]);
// The same file list `npm test` runs in companion-service/package.json.
const files = require("../companion-service/package.json").scripts.test.split(/\s+/).filter((a) => a.endsWith(".test.js"));
run(process.execPath, ["--test", "--test-concurrency=2", ...files]);
fs.rmSync(dest, { recursive: true, force: true });
