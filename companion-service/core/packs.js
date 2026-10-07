// Packs: how features reach the framework. A pack is a named list of features;
// each feature carries everything the rest of the service needs to know about
// it, so adding one means writing its own folder and listing it in a pack —
// not editing the registry, server.ts, the MCP catalog, the scope-key rules, the
// job persistence list and the history rules one by one.
//
//   feature: {
//     descriptor     setup/Settings facing: id, label, description, requiredChecks, needsRepos,
//                    promptSetup, ... (core/feature-registry.js documents the fields)
//     factory        () => (config, deps) => Feature — a thunk so setup.js and doctor.js, which
//                    run without a build, never load the implementation
//     mcpTools       catalog entries (core/mcp-tools.js) this feature owns; each names this feature
//     scopeKey       how a job of this feature is tagged: "pr" (a pull request), "jira-issue", or
//                    (payload) => string | null. Absent: its jobs carry no scope key
//     persist        false = jobs are not written to disk (default true)
//     extension      { script, order } — the content script (relative to chrome-extension/) that
//                    registers this feature's ✨ row, and where in the menu it sits (core/extension-manifest.js
//                    lists the scripts in this order)
//     history        { readOnly, milestone, eventOnly } — how core/history-record.js treats its jobs
//   }
//
// A pack can also contribute, beyond features:
//   siteSettings  { <siteId>: { <key>: { validate(value) -> {ok, value} | {ok: false, reason},
//                  defaultValue, from(config) -> the effective value } } } — extra keys on a
//                  site's settings (core/settings.js validates, saves and shows them like the
//                  site's own fields; a value equal to defaultValue is not saved)
//   targets       (config) -> object merged into GET /targets for the extension
//   externalTokens { label, sources: [({ home, fsImpl, readText, readJson, parseEnvFile, originOf }) ->
//                  { "<site>.apiToken": { token, username?, origin, source } }] } — tokens another tool on
//                  this machine already holds, used only when none is saved here and only for the
//                  host the entry's origin names (core/external-tokens.js)
//   diagnose      { available() -> bool, args(buildUrl) -> argv for `claude`, summary(build) -> text } —
//                  a richer way to diagnose a failed build than the plain prompt (Diagnose build)
//   doctorChecks  [({ config }) -> { status: "OK"|"FAIL", message } | null] extra `companion doctor` lines
//   mcpDenyList   [tool name] MCP tools a read-only Claude run must never be given
//   analysisServerHints [{ server, whenAvailable, whenMissing }] a user-scope MCP server the issue analysis
//                  should lean on when it is configured (prompt line for each case); the report shows a
//                  "<server> used" badge for it
//
// Plain JS and free of I/O beyond requiring the pack files: setup.js, doctor.js
// and the MCP catalog read it directly. Which packs are loaded is the
// environment profile's `packs` list (environment.js).
const path = require("path");

// "pr" is a pull request of the distribution's git host; "bitbucket-pr" is its old name.
const SCOPE_KEY_KINDS = ["pr", "bitbucket-pr", "jira-issue"];
const ID_RE = /^[a-z][a-z0-9-]*$/;

function fail(where, message) {
  throw new Error(`Invalid pack${where ? ` (${where})` : ""}: ${message}`);
}

/** Validates one feature spec; returns it unchanged. */
function checkFeature(packId, spec) {
  const id = spec && spec.descriptor && spec.descriptor.id;
  if (typeof id !== "string" || !ID_RE.test(id)) fail(packId, `a feature needs descriptor.id (lowercase, dashes), got ${JSON.stringify(id)}`);
  const where = `${packId}/${id}`;
  if (typeof spec.descriptor.label !== "string" || !spec.descriptor.label) fail(where, "descriptor.label is required");
  if (typeof spec.factory !== "function") fail(where, "factory must be a function returning the feature's create function");
  if (spec.scopeKey !== undefined && typeof spec.scopeKey !== "function" && !SCOPE_KEY_KINDS.includes(spec.scopeKey)) {
    fail(where, `scopeKey must be a function or one of ${SCOPE_KEY_KINDS.join(", ")}`);
  }
  if (spec.persist !== undefined && typeof spec.persist !== "boolean") fail(where, "persist must be a boolean");
  if (spec.extension !== undefined) {
    const { script, order } = spec.extension || {};
    if (typeof script !== "string" || !/^[A-Za-z0-9._-]+(\/[A-Za-z0-9._-]+)*\.js$/.test(script) || script.split("/").includes("..")) {
      fail(where, "extension.script must be a relative .js path inside the extension folder");
    }
    if (order !== undefined && typeof order !== "number") fail(where, "extension.order must be a number");
  }
  for (const tool of spec.mcpTools || []) {
    if (!tool || typeof tool.name !== "string") fail(where, "every mcpTools entry needs a name");
    if (tool.featureId !== id) fail(where, `MCP tool ${tool.name} must name this feature (featureId: "${id}"), not ${JSON.stringify(tool.featureId)}`);
  }
  return spec;
}

/** Validates and returns a pack. Throws on a duplicate feature id inside the pack. */
function definePack(pack) {
  if (!pack || typeof pack.id !== "string" || !pack.id) fail("", "a pack needs an id");
  if (!Array.isArray(pack.features)) fail(pack.id, "features must be an array");
  if (pack.targets !== undefined && typeof pack.targets !== "function") fail(pack.id, "targets must be a function (config) => object");
  if (pack.externalTokens !== undefined) {
    const t = pack.externalTokens;
    if (!t || typeof t.label !== "string" || !Array.isArray(t.sources) || !t.sources.every((fn) => typeof fn === "function")) {
      fail(pack.id, "externalTokens needs a label and a list of source functions");
    }
  }
  if (pack.diagnose !== undefined) {
    const d = pack.diagnose;
    if (!d || typeof d.available !== "function" || typeof d.args !== "function" || typeof d.summary !== "function") {
      fail(pack.id, "diagnose needs available(), args() and summary()");
    }
  }
  if (pack.doctorChecks !== undefined && !(Array.isArray(pack.doctorChecks) && pack.doctorChecks.every((fn) => typeof fn === "function"))) {
    fail(pack.id, "doctorChecks must be a list of functions");
  }
  if (pack.mcpDenyList !== undefined && !(Array.isArray(pack.mcpDenyList) && pack.mcpDenyList.every((n) => typeof n === "string" && n))) {
    fail(pack.id, "mcpDenyList must be a list of tool names");
  }
  if (
    pack.analysisServerHints !== undefined &&
    !(
      Array.isArray(pack.analysisServerHints) &&
      pack.analysisServerHints.every(
        (h) => h && typeof h.server === "string" && h.server && typeof h.whenAvailable === "string" && typeof h.whenMissing === "string",
      )
    )
  ) {
    fail(pack.id, "analysisServerHints must be a list of { server, whenAvailable, whenMissing }");
  }
  for (const [siteId, extras] of Object.entries(pack.siteSettings || {})) {
    for (const [key, extra] of Object.entries(extras || {})) {
      if (!extra || typeof extra.validate !== "function" || typeof extra.from !== "function" || extra.defaultValue === undefined) {
        fail(pack.id, `siteSettings.${siteId}.${key} needs validate(), from() and defaultValue`);
      }
    }
  }
  const seen = new Set();
  for (const spec of pack.features) {
    checkFeature(pack.id, spec);
    if (seen.has(spec.descriptor.id)) fail(pack.id, `feature "${spec.descriptor.id}" is listed twice`);
    seen.add(spec.descriptor.id);
  }
  return pack;
}

/** Resolves pack files against the service root (the folder that holds environment.js). */
function requirePack(file) {
  return require(path.join(__dirname, "..", file));
}

/** `packs` as given, with feature ids unique across all of them. */
function mergePacks(packs) {
  const owner = new Map();
  for (const pack of packs) {
    for (const spec of pack.features) {
      const id = spec.descriptor.id;
      if (owner.has(id)) fail(pack.id, `feature "${id}" is already provided by pack "${owner.get(id)}"`);
      owner.set(id, pack.id);
    }
  }
  return packs;
}

/** The packs named by `files` (relative to the service root), validated and merged. */
function loadPacks(files) {
  return mergePacks(files.map((file) => definePack(requirePack(file))));
}

let loaded;

/** The packs of the environment profile, loaded once. */
function allPacks() {
  if (!loaded) loaded = loadPacks(require("../environment.js").packs);
  return loaded;
}

/** Every feature spec, in pack order then declared order. */
function features() {
  return allPacks().flatMap((pack) => pack.features);
}

/** The spec of feature `id`, or undefined. */
function featureSpec(id) {
  return features().find((spec) => spec.descriptor.id === id);
}

/** Feature id -> (config, deps) => Feature. The implementation is only loaded when the
 * factory is called, i.e. for enabled features. */
function featureFactories() {
  const out = {};
  for (const spec of features()) {
    out[spec.descriptor.id] = (config, deps) => spec.factory()(config, deps);
  }
  return out;
}

/** The MCP catalog entries of every feature, in feature order. */
function mcpToolDefs() {
  return features().flatMap((spec) => spec.mcpTools || []);
}

/** The extra settings keys packs add to site `siteId`: [{ key, validate, defaultValue, from }]. */
function siteSettingExtras(siteId) {
  return allPacks().flatMap((pack) =>
    Object.entries((pack.siteSettings && pack.siteSettings[siteId]) || {}).map(([key, extra]) => ({ key, ...extra })),
  );
}

/** What the packs add to GET /targets for this config. */
function targetsFor(config) {
  return Object.assign({}, ...allPacks().map((pack) => (pack.targets ? pack.targets(config) : {})));
}

/** Every source function of the packs' external-token readers. */
function externalTokenSources() {
  return allPacks().flatMap((pack) => (pack.externalTokens ? pack.externalTokens.sources : []));
}

/** What to call the tool whose tokens are borrowed, for messages. */
function externalTokenLabel() {
  const pack = allPacks().find((p) => p.externalTokens);
  return pack ? pack.externalTokens.label : "another tool on this machine";
}

/** The first pack-provided way to diagnose a build that is usable right now, or null. */
function diagnoseCommand() {
  for (const pack of allPacks()) {
    if (pack.diagnose && pack.diagnose.available()) return pack.diagnose;
  }
  return null;
}

/** The packs' extra doctor checks. */
function doctorChecks() {
  return allPacks().flatMap((pack) => pack.doctorChecks || []);
}

/** MCP tool names the packs say a read-only run must be denied. */
function disallowedMcpTools() {
  return allPacks().flatMap((pack) => pack.mcpDenyList || []);
}

/** The packs' hints about MCP servers the issue analysis should prefer. */
function analysisServerHints() {
  return allPacks().flatMap((pack) => pack.analysisServerHints || []);
}

/** Ids of the features whose spec says `history[flag]`. */
function idsWithHistoryFlag(flag) {
  return features()
    .filter((spec) => spec.history && spec.history[flag] === true)
    .map((spec) => spec.descriptor.id);
}

/** Ids of the features whose jobs are not written to disk. */
function notPersistedIds() {
  return features()
    .filter((spec) => spec.persist === false)
    .map((spec) => spec.descriptor.id);
}

/** The scope key of a job of `featureId` started with `payload`, or null when the feature
 * has no rule. `kinds` maps the named rules to their implementations (core/scope-key.js). */
function scopeKeyFor(featureId, payload, kinds) {
  const rule = featureSpec(featureId)?.scopeKey;
  if (typeof rule === "function") return rule(payload);
  if (typeof rule === "string") return kinds[rule](payload);
  return null;
}

module.exports = {
  definePack,
  mergePacks,
  loadPacks,
  features,
  featureSpec,
  featureFactories,
  mcpToolDefs,
  siteSettingExtras,
  targetsFor,
  externalTokenSources,
  externalTokenLabel,
  diagnoseCommand,
  doctorChecks,
  disallowedMcpTools,
  analysisServerHints,
  idsWithHistoryFlag,
  notPersistedIds,
  scopeKeyFor,
};
