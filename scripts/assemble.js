#!/usr/bin/env node
// Builds the folder layout the companion actually runs from — companion-service/, chrome-extension/,
// plugin/, plus a distribution's own root files — out of the framework and the distribution that
// uses it:
//
//   frameworkDir   this repo (or a copy of it in node_modules): companion-service/, chrome-extension/, plugin/
//   rootDir        the distribution's checkout: its installer, README, changelog, docs, marketplace file...
//   overlays       folders laid over the result, in order, mirroring that layout: a distribution's own
//                  environment profile (companion-service/environment.js replaces the framework's
//                  placeholder), its packs, its plugin manifest
//
// The installer mirrors the assembled tree into the install folder, and a distribution's tests run in
// it, so what is tested is exactly what gets installed. Only core fs calls are used: an installer runs
// this before it has checked the Node version.
const fs = require("fs");
const path = require("path");

/** The framework folders that make up a running tree; everything else in this repo is the framework's own tooling. */
const FRAMEWORK_DIRS = ["companion-service", "chrome-extension", "plugin"];

// Never part of an assembled tree, at any depth.
const NEVER = new Set(["node_modules", "dist", ".git", ".DS_Store", ".assembled", ".assembled-framework", "releases"]);
// This machine's own state: it lives next to the code in a working checkout and must not travel.
const LOCAL_STATE = [
  // Scratch checkouts of other branches made by Claude Code; not part of the product.
  /^\.claude\/worktrees(\/|$)/,
  /^companion-service\/config\.json$/,
  /^companion-service\/credentials\.enc$/,
  /^companion-service\/analysis-cache(\/|$)/,
  /^companion-service\/summary-cache(\/|$)/,
  /^companion-service\/install-source\.json$/,
  /^chrome-extension\/companion-config\.js$/,
  /\.log$/,
];

function skipped(rel, name, extraSkip) {
  return NEVER.has(name) || LOCAL_STATE.some((re) => re.test(rel)) || extraSkip.some((re) => re.test(rel));
}

/** Copies `src` into `dest` (merging directories, overwriting files), leaving out what `skipped` names. */
function copyInto(src, dest, rel, extraSkip) {
  const stat = fs.statSync(src);
  if (stat.isDirectory()) {
    fs.mkdirSync(dest, { recursive: true });
    for (const name of fs.readdirSync(src)) {
      const childRel = rel ? `${rel}/${name}` : name;
      if (skipped(childRel, name, extraSkip)) continue;
      copyInto(path.join(src, name), path.join(dest, name), childRel, extraSkip);
    }
    return;
  }
  if (fs.existsSync(dest) && fs.statSync(dest).isDirectory()) fs.rmSync(dest, { recursive: true, force: true });
  fs.copyFileSync(src, dest);
  fs.chmodSync(dest, stat.mode);
}

/**
 * Assembles into `dest` (emptied first) and returns it.
 *   frameworkDir  where companion-service/, chrome-extension/ and plugin/ come from
 *   rootDir       (optional) the distribution's root files and folders, copied first
 *   rootSkip      (optional) names at the top of rootDir to leave out (its overlay folders, its scripts' scratch...)
 *   overlays      (optional) folders merged over the result, last one wins
 *   notInstalled  (optional) regexes of paths (relative to the tree) to leave out of the result
 *   version       (optional) the distribution's version: written to the service's package.json, the extension's
 *                 manifest and the plugin manifest, so everything installed from the tree reports one version
 *                 (the update check compares them)
 *   Afterwards the extension's manifest.json is regenerated for the assembled profile and packs
 *   (core/extension-manifest.js), so its hosts and script list are right before setup ever runs.
 */
function assemble({ frameworkDir, dest, rootDir, rootSkip = [], overlays = [], notInstalled = [], version }) {
  fs.rmSync(dest, { recursive: true, force: true });
  fs.mkdirSync(dest, { recursive: true });
  const skipRoot = new Set(rootSkip);
  if (rootDir) {
    for (const name of fs.readdirSync(rootDir)) {
      if (skipRoot.has(name) || skipped(name, name, notInstalled)) continue;
      copyInto(path.join(rootDir, name), path.join(dest, name), name, notInstalled);
    }
  }
  for (const name of FRAMEWORK_DIRS) {
    const dir = path.join(frameworkDir, name);
    if (fs.existsSync(dir)) copyInto(dir, path.join(dest, name), name, notInstalled);
  }
  for (const overlay of overlays) {
    if (fs.existsSync(overlay)) copyInto(overlay, dest, "", notInstalled);
  }
  if (version) stampVersion(dest, version);
  regenerateManifest(dest);
  stampPluginSlug(dest);
  return dest;
}

/** Sets "version" in the JSON files that carry the product's version, where they exist. */
function stampVersion(dest, version) {
  for (const rel of ["companion-service/package.json", "chrome-extension/manifest.json", "plugin/.claude-plugin/plugin.json"]) {
    const file = path.join(dest, rel);
    if (!fs.existsSync(file)) continue;
    const json = JSON.parse(fs.readFileSync(file, "utf8"));
    json.version = version;
    fs.writeFileSync(file, `${JSON.stringify(json, null, 2)}\n`);
  }
}

/** Rewrites the assembled extension's manifest from the assembled profile and packs, using the tree's own code. */
function regenerateManifest(dest) {
  const tool = path.join(dest, "companion-service", "core", "extension-manifest.js");
  const extensionDir = path.join(dest, "chrome-extension");
  if (!fs.existsSync(tool) || !fs.existsSync(path.join(extensionDir, "manifest.json"))) return;
  require(tool).writeManifest(extensionDir, {});
}

/** Writes the profile's app slug next to the plugin's MCP launcher, which can't read the profile itself:
 * it needs it to find the installed companion (~/<slug>). */
function stampPluginSlug(dest) {
  const slugModule = path.join(dest, "companion-service", "core", "app-slug.js");
  const binDir = path.join(dest, "plugin", "bin");
  if (!fs.existsSync(slugModule) || !fs.existsSync(binDir)) return;
  fs.writeFileSync(path.join(binDir, "app-slug.json"), `${JSON.stringify({ appSlug: require(slugModule).APP_SLUG }, null, 2)}\n`);
}

module.exports = { assemble, FRAMEWORK_DIRS };
