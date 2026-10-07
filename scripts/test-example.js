#!/usr/bin/env node
// Assembles the framework with the example profile (examples/minimal-env), builds it and runs the example's own
// test in it: what a distribution that names GitHub as its git host gets.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");
const { assemble } = require("./assemble.js");

const root = path.join(__dirname, "..");
const dest = fs.mkdtempSync(path.join(os.tmpdir(), "ai-dev-companion-example-"));
const tree = path.join(dest, "tree");
assemble({ frameworkDir: root, overlays: [path.join(root, "examples", "minimal-env")], dest: tree, version: "0.0.0-example" });
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
run(process.execPath, ["--test", "github-profile.test.js"]);
fs.rmSync(dest, { recursive: true, force: true });
