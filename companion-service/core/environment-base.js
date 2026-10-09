// Builds an environment profile from plain data. A profile is everything that says "this is
// one team's setup" rather than "this is how the tool works": the servers it talks to, where
// updates come from, which LLM proxy and Jira projects to default to, which packs to load,
// what to call things. A distribution supplies one as companion-service/environment.js (the
// framework ships a neutral placeholder one), written as
//
//   const { defineEnvironment } = require("./core/environment-base.js");
//   module.exports = defineEnvironment({ sites, updateSource, llm, targets, issues, branding, packs });
//
// Code elsewhere reads these values through the helpers below and never hardcodes a host, a
// job name or a model. Layers, lowest to highest precedence: the profile, then config.json;
// the profile only supplies defaults, nothing in it is a hard limit.
//
// Plain JS and dependency-free: setup.js, doctor.js and the registry require it without a build.

const SITE_KINDS = ["git", "issues", "ci"];
// The --permission-mode values a Claude Code session can be started (and resumed) with.
const PERMISSION_MODES = ["plan", "auto", "default"];

function fail(message) {
  throw new Error(`Invalid environment profile: ${message}`);
}

/** Checks a profile's optional `claude` section: `permissionMode`, and `permissionModes` keyed by feature id. */
function validateClaudeSection(claude) {
  if (claude === undefined) return;
  if (!claude || typeof claude !== "object") fail("claude must be an object");
  const check = (what, v) => {
    if (!PERMISSION_MODES.includes(v)) fail(`claude.${what} must be one of ${PERMISSION_MODES.join(", ")}, got ${JSON.stringify(v)}`);
  };
  if (claude.permissionMode !== undefined) check("permissionMode", claude.permissionMode);
  for (const [id, mode] of Object.entries(claude.permissionModes || {})) check(`permissionModes.${id}`, mode);
}

/** Checks `profile` and returns it with the lookup helpers added. */
function defineEnvironment(profile) {
  if (!profile || typeof profile !== "object") fail("expected an object");
  const { sites, packs } = profile;
  validateClaudeSection(profile.claude);
  if (!Array.isArray(sites) || sites.length === 0) fail("sites must be a non-empty list");
  const seen = new Set();
  for (const site of sites) {
    if (!site || typeof site.id !== "string" || !/^[a-z][a-z0-9]*$/.test(site.id)) fail(`a site needs a lowercase id, got ${JSON.stringify(site && site.id)}`);
    if (seen.has(site.id)) fail(`site "${site.id}" is listed twice`);
    seen.add(site.id);
    if (!SITE_KINDS.includes(site.kind)) fail(`site "${site.id}" has kind ${JSON.stringify(site.kind)}, expected one of ${SITE_KINDS.join(", ")}`);
    if (typeof site.provider !== "string" || !site.provider) fail(`site "${site.id}" needs a provider`);
    try {
      new URL(site.baseUrl);
    } catch {
      fail(`site "${site.id}" needs a baseUrl that is a URL`);
    }
    if (site.tokenOnly !== undefined && typeof site.tokenOnly !== "boolean") fail(`site "${site.id}": tokenOnly must be true or false`);
    if (site.pageMatches !== undefined) {
      const ok = Array.isArray(site.pageMatches) && site.pageMatches.length > 0 && site.pageMatches.every((m) => typeof m === "string" && m.startsWith("{origin}/"));
      if (!ok) fail(`site "${site.id}": pageMatches must be a list of patterns starting with "{origin}/", e.g. "{origin}/*/*/pull/*"`);
    }
  }
  if (!Array.isArray(packs) || packs.some((p) => typeof p !== "string" || !p)) fail("packs must be a list of pack files");

  const siteById = (id) => sites.find((s) => s.id === id);
  return {
    ...profile,
    /** Every site id, in profile order. */
    siteIds: () => sites.map((s) => s.id),
    /** The profile entry for `id`, or undefined. */
    siteById,
    /** The default base URL of site `id` ("" when the profile has no such site). */
    defaultBaseUrl: (id) => (siteById(id) ? siteById(id).baseUrl : ""),
    /** The credential names that belong to sites: `jira.apiToken`, … */
    siteCredentialNames: () => sites.map((s) => `${s.id}.apiToken`),
  };
}

module.exports = { defineEnvironment, validateClaudeSection, SITE_KINDS, PERMISSION_MODES };
