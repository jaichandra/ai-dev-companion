// The extension's manifest.json, generated: the hosts it runs on come from the
// environment profile's sites (with any base URL overridden in config.json), and the
// content-script list from the packs' features — so a different Jira host or a
// different set of features never means hand-editing the manifest.
//
// The checked-in chrome-extension/manifest.json is the output for the default
// profile and config (a test keeps the two equal); setup.js rewrites it in the copy
// it runs from, so an install or update always ends with the right hosts. Only the
// host lists, the script list and the name (the profile's product name) are generated: the version, permissions and
// background worker are read back from the existing file, so a version bump stays
// a one-place edit. Plain JS with no dependencies beyond the profile and packs
// (setup.js and doctor.js require it without a build).
const fs = require("fs");
const path = require("path");
const environment = require("../environment.js");
const packs = require("./packs.js");
const { PRODUCT_NAME } = require("./product-name.js");

/** Scripts every install loads, in order, before any feature file: they share one
 * global scope, and the registry must exist before the features register. */
const BASE_SCRIPTS = ["diff-viewer.js", "open-in-editor.js", "settings-panel.js", "target-patterns.js", "jenkins-urls.js", "registry.js", "ui-kit.js", "jira-helpers.js", "git-host.js"];
/** Loaded last: it reads the registry once at startup. */
const FINAL_SCRIPT = "content.js";
/** The service the extension talks to; any port. */
const LOCAL_HOST = "http://127.0.0.1/*";

function originOf(url) {
  try {
    const u = new URL(url);
    return u.protocol === "https:" || u.protocol === "http:" ? u.origin : null;
  } catch {
    return null;
  }
}

/** The origin of every site, with config.json's base URL overriding the profile's. */
function siteOrigins(config) {
  const out = {};
  for (const site of environment.sites) {
    const configured = config && config[site.id] && config[site.id].baseUrl;
    out[site.id] = originOf(configured || site.baseUrl);
  }
  return out;
}

/** The origin of the site of each kind ("git" | "issues" | "ci"), for GET /targets: the extension
 * skips a feature on a page of any other origin. */
function originsByKind(config) {
  const origins = siteOrigins(config);
  const out = {};
  for (const site of environment.sites) {
    if (!(site.kind in out) && origins[site.id]) out[site.kind] = origins[site.id];
  }
  return out;
}

/** The kind-by-kind provider ids of the profile ({ git: "github", issues: "jira-dc", ci: "jenkins" }), for GET /targets:
 * the extension reads pull request pages the way that git host lays them out. */
function providersByKind() {
  const out = {};
  for (const site of environment.sites) {
    if (!(site.kind in out)) out[site.kind] = site.provider;
  }
  return out;
}

/** Host permissions (`https://host/*`), de-duplicated, in profile order. A token-only site (GitHub) is left
 * out: its browser login is no use to the companion, so the extension is given no reason to read its cookies. */
function hostPatterns(config) {
  const origins = siteOrigins(config);
  const hosts = environment.sites.filter((s) => !s.tokenOnly).map((s) => origins[s.id]);
  return [...new Set(hosts.filter(Boolean))].map((origin) => `${origin}/*`);
}

/** The match patterns of a site's pages for the extension: the whole origin, or the profile's `pageMatches`
 * (e.g. only pull request pages on GitHub) with `{origin}` replaced by the effective origin. */
function pagePatterns(site, origin) {
  if (!origin) return [];
  return (site.pageMatches || ["{origin}/*"]).map((pattern) => pattern.replace("{origin}", origin));
}

/** Where the content scripts run: every site's pages (narrowed by `pageMatches`), de-duplicated. */
function matchPatterns(config) {
  const origins = siteOrigins(config);
  return [...new Set(environment.sites.flatMap((site) => pagePatterns(site, origins[site.id])))];
}

/** Match patterns of the sites whose pages show stack traces (git hosts and CI), for the
 * "Open in editor" right-click menu; the issue tracker is left out on purpose. */
function contextMenuMatches(config) {
  const origins = siteOrigins(config);
  const wanted = environment.sites.filter((s) => s.kind === "git" || s.kind === "ci");
  return [...new Set(wanted.flatMap((site) => pagePatterns(site, origins[site.id])))];
}

/** Feature scripts, in the ✨ menu's order: the features' `extension.order`, ties by pack order. */
function featureScripts() {
  return packs
    .features()
    .map((spec, index) => ({ spec, index }))
    .filter(({ spec }) => spec.extension && spec.extension.script)
    .sort((a, b) => (a.spec.extension.order ?? 1000) - (b.spec.extension.order ?? 1000) || a.index - b.index)
    .map(({ spec }) => spec.extension.script);
}

/** Every script of the content-script list, in load order. */
function contentScripts() {
  return [...BASE_SCRIPTS, ...featureScripts(), FINAL_SCRIPT];
}

/** `base` (the existing manifest) with the generated fields filled in. */
function buildManifest(base, config) {
  const manifest = { ...base, name: PRODUCT_NAME, host_permissions: [...hostPatterns(config), LOCAL_HOST] };
  const [first = {}, ...rest] = base.content_scripts || [];
  manifest.content_scripts = [{ ...first, matches: matchPatterns(config), js: contentScripts() }, ...rest];
  return manifest;
}

function render(manifest) {
  return `${JSON.stringify(manifest, null, 2)}\n`;
}

/** The chrome-extension folder next to the service, whether running from source or from dist/. */
function defaultExtensionDir() {
  const serviceDir = path.basename(path.join(__dirname, "..")) === "dist" ? path.join(__dirname, "..", "..") : path.join(__dirname, "..");
  return path.join(serviceDir, "..", "chrome-extension");
}

/** The generated file background.js loads via importScripts — kept out of background.js itself so
 * updating the extension's code can never reset the secret to a placeholder, and the secret never
 * lands in git. */
function renderExtensionConfig(secret, port, contextMenuMatches = []) {
  const value = { serviceBaseUrl: `http://127.0.0.1:${port}`, sharedSecret: secret, contextMenuMatches };
  return (
    "// Generated by companion-service/setup.js — do not edit or commit.\n" +
    "// Must match companion-service/config.json; re-run `npm run setup` to regenerate.\n" +
    `self.COMPANION_CONFIG = ${JSON.stringify(value, null, 2)};\n`
  );
}

/** Whether the hosts the extension must cover differ between two configs. */
function hostsChanged(before, after) {
  const shape = (config) => JSON.stringify([hostPatterns(config), matchPatterns(config)]);
  return shape(before) !== shape(after);
}

/** Writes what the extension folder needs from `config`: companion-config.js (service address, secret,
 * right-click menu hosts) and a manifest whose hosts and scripts match. Returns whether the manifest changed. */
function writeExtensionFiles(extensionDir, config) {
  fs.writeFileSync(path.join(extensionDir, "companion-config.js"), renderExtensionConfig(config.sharedSecret, config.port, contextMenuMatches(config)));
  return writeManifest(extensionDir, config);
}

/** Rewrites <extensionDir>/manifest.json when its generated fields differ. Returns true if it changed. */
function writeManifest(extensionDir, config) {
  const file = path.join(extensionDir, "manifest.json");
  const current = fs.readFileSync(file, "utf8");
  const next = render(buildManifest(JSON.parse(current), config));
  if (next === current) return false;
  fs.writeFileSync(file, next);
  return true;
}

/** Problems between the manifest on disk and what config + packs call for ([] when it matches). */
function checkManifest(extensionDir, config) {
  const file = path.join(extensionDir, "manifest.json");
  let current;
  try {
    current = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (err) {
    return [`${file} can't be read (${err.message}).`];
  }
  const want = buildManifest(current, config);
  const problems = [];
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  if (!same(current.host_permissions, want.host_permissions)) problems.push(`host_permissions should be ${want.host_permissions.join(", ")}.`);
  const have = (current.content_scripts && current.content_scripts[0]) || {};
  const wantFirst = want.content_scripts[0];
  if (!same(have.matches, wantFirst.matches)) problems.push(`content_scripts.matches should be ${wantFirst.matches.join(", ")}.`);
  if (!same(have.js, wantFirst.js)) problems.push("content_scripts.js doesn't list the features of the loaded packs.");
  for (const script of wantFirst.js) {
    if (!fs.existsSync(path.join(extensionDir, script))) problems.push(`${script} is missing from the extension folder.`);
  }
  return problems;
}

module.exports = {
  BASE_SCRIPTS,
  FINAL_SCRIPT,
  defaultExtensionDir,
  renderExtensionConfig,
  hostsChanged,
  writeExtensionFiles,
  LOCAL_HOST,
  siteOrigins,
  originsByKind,
  providersByKind,
  hostPatterns,
  matchPatterns,
  contextMenuMatches,
  featureScripts,
  contentScripts,
  buildManifest,
  render,
  writeManifest,
  checkManifest,
};
