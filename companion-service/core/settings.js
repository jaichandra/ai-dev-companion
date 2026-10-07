// Everything behind the extension's Settings panel (server.ts's
// GET/PUT /settings) that doesn't need a running server: what the panel
// may see, what it may change, and how a change is merged into
// config.json. Plain JS, not TypeScript — same reason as core/prereqs.js:
// settings.test.js runs it directly, and setup.js shares writeConfigFile
// with zero build step.
//
// The panel is the in-browser counterpart of setup.js's wizard, minus the
// two settings that connect the extension to this service at all: `port`
// and `sharedSecret` are never accepted from it (changing either from the
// browser would cut off the very extension that asked), and the secret —
// like every API token — is never sent back out.
const fs = require("fs");
const path = require("path");
const environment = require("../environment.js");
const registry = require("./feature-registry.js");
const packs = require("./packs.js");
const prereqs = require("./prereqs.js");
const historySchema = require("./history-schema.js");
const targets = require("./targets.js");
const background = require("./settings-background.js");

const EDITABLE_KEYS = [
  "enabledFeatures",
  "repos",
  "reviewEditor",
  ...environment.siteIds(),
  "analyzeIssue",
  "acknowledgeWarnings",
  "sessionCache",
  "history",
  "riskFacts",
  "ticketToPr",
  "summarizeComments",
  "addressReviewComments",
  "mcp",
  ...background.BACKGROUND_KEYS,
];
const LOCKED_KEYS = ["port", "sharedSecret"];
const TICKET_TO_PR_TRANSITION_DEFAULT = environment.issues.reviewTransitionName;
const MAX_REPOS = 500;
const MAX_TOKEN_LENGTH = 1000;
// core/session-vault.js's in-memory cache of relayed browser sessions.
// ttlMinutes 0 turns it off entirely (a meaningful, persisted value, not
// "unset"); the cap keeps a mistyped setting from caching a session for
// weeks. heartbeat off by default — see core/auth-context.ts's
// heartbeatHosts for what turning it on actually does.
const SESSION_CACHE_TTL_MINUTES_DEFAULT = 30;
const SESSION_CACHE_HEARTBEAT_DEFAULT = false;
const MAX_SESSION_CACHE_TTL_MINUTES = 1440;
// The /mcp route's settings (core/mcp.ts). rotateToken is a one-shot
// command, not a setting: it becomes an mcp.token tokenOp in mergeSettings
// and is never written to config.json.
const MCP_TOKEN_NAME = "mcp.token";
// Passed to `claude --model` or `cursor-agent --model` as a single execFile
// argument (never a shell), but still limited to what model ids and aliases
// actually look like, e.g. "claude-sonnet-5", "opus", "claude-opus-4-1[1m]",
// "claude-opus-4-8[context=1m,effort=high]".
const MODEL_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:[\]=,-]{0,99}$/;
const REPO_KEY_PATTERN = /^([^/]+)\/([^/]+)$/;

function isPlainObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasControlChars(value) {
  // eslint-disable-next-line no-control-regex
  return /[\u0000-\u001f\u007f]/.test(value);
}

function isHttpUrl(value) {
  try {
    const url = new URL(value);
    return (url.protocol === "http:" || url.protocol === "https:") && !!url.hostname;
  } catch {
    return false;
  }
}

/**
 * The config as the Settings panel may see it: tokens reduced to whether
 * one is saved, sharedSecret left out entirely, port only as a read-only
 * value. Tokens themselves never live in config.json (see
 * migrateTokensToStore below and Task 3) — `apiTokenSet` comes from
 * `credentials.list()` instead of a config field. `credentials` is
 * `{ list }` — in production, core/credentials.ts's `listTokenNames`
 * (which already turns an undecryptable store into `[]` rather than
 * throwing — see core/token-cache.js — so a corrupt credentials.enc
 * degrades GET /settings to "nothing saved" instead of a 500).
 */
function publicSettings(config, credentials) {
  const savedTokenNames = credentials.list();
  // Where a site's token would come from when none is saved (another tool's files, per the packs), else "".
  const external = (typeof credentials.externalSources === "function" ? credentials.externalSources(config) : null) || {};
  const externalFor = (site) => (savedTokenNames.includes(`${site}.apiToken`) ? "" : external[site] || "");
  return {
    port: config.port,
    enabledFeatures: registry.enabledFeatureIds(config),
    repos: { ...(config.repos || {}) },
    // A saved "vscode" (hidden for now) shows as automatic.
    reviewEditor: config.reviewEditor === "cursor" || config.reviewEditor === "claude-code" ? config.reviewEditor : null,
    // One entry per site of the profile (jira, jenkins, bitbucket or github, ...), each keyed by its id.
    ...Object.fromEntries(
      environment.sites.map((site) => [
        site.id,
        {
          baseUrl: config[site.id]?.baseUrl || "",
          ...(site.withUsername ? { username: config[site.id]?.username || "" } : {}),
          apiTokenSet: savedTokenNames.includes(`${site.id}.apiToken`),
          apiTokenExternal: externalFor(site.id),
          ...Object.fromEntries(packs.siteSettingExtras(site.id).map((extra) => [extra.key, extra.from(config)])),
        },
      ]),
    ),
    analyzeIssue: {
      model: config.analyzeIssue?.model || "",
      ...targets.analyzeTargetsFrom(config),
      componentRepoMap: { ...(config.analyzeIssue?.componentRepoMap || {}) },
    },
    history: { retentionDays: config.history?.retentionDays ?? historySchema.DEFAULT_RETENTION_DAYS },
    riskFacts: { url: config.riskFacts?.url || "" },
    summarizeComments: { model: config.summarizeComments?.model || "" },
    addressReviewComments: { model: config.addressReviewComments?.model || "" },
    ticketToPr: {
      reviewTransitionName: config.ticketToPr?.reviewTransitionName || TICKET_TO_PR_TRANSITION_DEFAULT,
      autoMoveToReview: config.ticketToPr?.autoMoveToReview !== false,
    },
    sessionCache: {
      ttlMinutes: config.sessionCache?.ttlMinutes ?? SESSION_CACHE_TTL_MINUTES_DEFAULT,
      heartbeat: config.sessionCache?.heartbeat ?? SESSION_CACHE_HEARTBEAT_DEFAULT,
    },
    mcp: {
      tokenSet: savedTokenNames.includes(MCP_TOKEN_NAME),
    },
    ...background.publicBackgroundSettings(config, savedTokenNames),
  };
}

/** What the panel shows for each feature — the registry descriptors minus
 * promptSetup, which only makes sense in a terminal. */
function featureSummaries() {
  return registry.FEATURE_DESCRIPTORS.map((d) => ({
    id: d.id,
    label: d.label,
    description: d.description,
    requiredChecks: d.requiredChecks,
    needsRepos: !!d.needsRepos,
    enabledByDefault: d.enabledByDefault !== false,
  }));
}

function validateModel(errors, field, value, { allowBlank }) {
  if (typeof value !== "string") {
    errors.push({ field, message: "must be a string." });
    return;
  }
  const trimmed = value.trim();
  if (!trimmed) {
    if (!allowBlank) errors.push({ field, message: "can't be blank." });
    return;
  }
  if (!MODEL_PATTERN.test(trimmed)) {
    errors.push({ field, message: `"${trimmed}" doesn't look like a model id (e.g. claude-sonnet-5).` });
  }
}

function validateSite(errors, prefix, value, { withUsername }) {
  if (!isPlainObject(value)) {
    errors.push({ field: prefix, message: "must be an object." });
    return;
  }
  const allowed = ["baseUrl", "apiToken", "clearApiToken", ...(withUsername ? ["username"] : [])];
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) errors.push({ field: `${prefix}.${key}`, message: "isn't a known setting." });
  }
  if (value.baseUrl !== undefined) {
    if (typeof value.baseUrl !== "string") {
      errors.push({ field: `${prefix}.baseUrl`, message: "must be a string." });
    } else if (value.baseUrl.trim() && !isHttpUrl(value.baseUrl.trim())) {
      errors.push({ field: `${prefix}.baseUrl`, message: "must be an http:// or https:// URL." });
    }
  }
  if (withUsername && value.username !== undefined) {
    if (typeof value.username !== "string" || hasControlChars(value.username) || value.username.length > 200) {
      errors.push({ field: `${prefix}.username`, message: "must be a short single-line string." });
    }
  }
  if (value.apiToken !== undefined) {
    if (typeof value.apiToken !== "string" || value.apiToken.length > MAX_TOKEN_LENGTH) {
      errors.push({ field: `${prefix}.apiToken`, message: "must be a string." });
    } else if (/\s/.test(value.apiToken.trim())) {
      errors.push({ field: `${prefix}.apiToken`, message: "can't contain spaces or line breaks." });
    }
  }
  if (value.clearApiToken !== undefined && typeof value.clearApiToken !== "boolean") {
    errors.push({ field: `${prefix}.clearApiToken`, message: "must be true or false." });
  }
}

/**
 * Checks a PUT /settings body against the config currently saved. Returns
 * [{ field, message }] — empty when the update may be saved. Every key is
 * optional (only the ones present change anything); `repos`, when present,
 * is the complete new map. A repo mapping that's new or points somewhere
 * new must be a real clone whose origin is that same PROJECT/repo — the
 * service runs git in it. `deps` is injectable for tests.
 */
function validateSettingsUpdate(update, current, deps = {}) {
  const checkRepoPath = deps.checkRepoPath || prereqs.checkRepoPath;
  const getOriginUrl = deps.getOriginUrl || prereqs.getOriginUrl;
  const errors = [];
  if (!isPlainObject(update)) return [{ field: "", message: "Settings must be a JSON object." }];

  for (const key of Object.keys(update)) {
    if (LOCKED_KEYS.includes(key)) {
      errors.push({
        field: key,
        message:
          "can't be changed from the browser — the extension would lose its connection to the service. " +
          "Re-run `npm run setup` in the companion-service folder instead.",
      });
    } else if (!EDITABLE_KEYS.includes(key)) {
      errors.push({ field: key, message: "isn't a known setting." });
    }
  }

  if (update.enabledFeatures !== undefined) {
    const known = new Set(registry.allFeatureIds());
    if (!Array.isArray(update.enabledFeatures)) {
      errors.push({ field: "enabledFeatures", message: "must be a list of feature ids." });
    } else {
      for (const id of update.enabledFeatures) {
        if (typeof id !== "string" || !known.has(id)) {
          errors.push({ field: "enabledFeatures", message: `"${id}" isn't a known feature.` });
        }
      }
      if (new Set(update.enabledFeatures).size !== update.enabledFeatures.length) {
        errors.push({ field: "enabledFeatures", message: "lists a feature more than once." });
      }
    }
  }

  if (update.repos !== undefined) {
    if (!isPlainObject(update.repos)) {
      errors.push({ field: "repos", message: 'must map "PROJECT/repo" to a folder.' });
    } else {
      const entries = Object.entries(update.repos);
      if (entries.length > MAX_REPOS) errors.push({ field: "repos", message: `can't list more than ${MAX_REPOS}.` });
      const currentRepos = current.repos || {};
      const seen = new Set();
      for (const [key, clonePath] of entries.slice(0, MAX_REPOS)) {
        const field = `repos.${key}`;
        const match = key.match(REPO_KEY_PATTERN);
        if (!match || !prereqs.isSafeRepoSegment(match[1]) || !prereqs.isSafeRepoSegment(match[2])) {
          errors.push({ field, message: 'must be "PROJECT/repo" (letters, digits, ".", "_" or "-").' });
          continue;
        }
        if (seen.has(key.toLowerCase())) {
          errors.push({ field, message: "is listed more than once." });
          continue;
        }
        seen.add(key.toLowerCase());
        if (typeof clonePath !== "string" || !clonePath.trim() || hasControlChars(clonePath)) {
          errors.push({ field, message: "needs the folder of its local clone." });
          continue;
        }
        if (!path.isAbsolute(clonePath.trim())) {
          errors.push({ field, message: "must be an absolute folder path (starting with /)." });
          continue;
        }
        const resolved = path.resolve(clonePath.trim());
        if (currentRepos[key] && path.resolve(currentRepos[key]) === resolved) continue;
        const check = checkRepoPath(resolved);
        if (!check.ok) {
          errors.push({ field, message: check.message });
          continue;
        }
        const origin = getOriginUrl(resolved);
        if (!prereqs.originMatchesProjectRepo(origin, match[1], match[2])) {
          errors.push({ field, message: `${resolved} is a clone of ${origin || "an unknown remote"}, not ${key}.` });
        }
      }
    }
  }

  if (update.reviewEditor !== undefined && update.reviewEditor !== null) {
    if (!registry.REVIEW_EDITORS.some((e) => e.id === update.reviewEditor)) {
      errors.push({
        field: "reviewEditor",
        message: `must be one of: ${registry.REVIEW_EDITORS.map((e) => e.id).join(", ")}.`,
      });
    }
  }

  for (const siteId of environment.siteIds()) {
    if (update[siteId] === undefined) continue;
    const withUsername = environment.siteById(siteId).withUsername === true;
    const extras = packs.siteSettingExtras(siteId);
    if (isPlainObject(update[siteId]) && extras.some((extra) => update[siteId][extra.key] !== undefined)) {
      // The pack's own keys are checked by the pack; the rest is the site's.
      const rest = { ...update[siteId] };
      for (const extra of extras) delete rest[extra.key];
      validateSite(errors, siteId, rest, { withUsername });
      for (const extra of extras) {
        const value = update[siteId][extra.key];
        if (value === undefined || value === null) continue;
        const checked = extra.validate(value);
        if (!checked.ok) errors.push({ field: `${siteId}.${extra.key}`, message: `${checked.reason}.` });
      }
    } else {
      validateSite(errors, siteId, update[siteId], { withUsername });
    }
  }

  if (update.analyzeIssue !== undefined) {
    if (!isPlainObject(update.analyzeIssue)) {
      errors.push({ field: "analyzeIssue", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.analyzeIssue)) {
        if (!["model", "projects", "issueTypes", "componentRepoMap"].includes(key)) {
          errors.push({ field: `analyzeIssue.${key}`, message: "isn't a known setting." });
        }
      }
      if (update.analyzeIssue.model !== undefined) {
        validateModel(errors, "analyzeIssue.model", update.analyzeIssue.model, { allowBlank: true });
      }
      for (const [key, validate] of [
        ["projects", targets.validateProjects],
        ["issueTypes", targets.validateIssueTypes],
        ["componentRepoMap", targets.validateComponentRepoMap],
      ]) {
        const value = update.analyzeIssue[key];
        if (value === undefined || value === null) continue;
        const checked = validate(value);
        if (!checked.ok) errors.push({ field: `analyzeIssue.${key}`, message: `${checked.reason}.` });
      }
    }
  }

  if (update.acknowledgeWarnings !== undefined && typeof update.acknowledgeWarnings !== "boolean") {
    errors.push({ field: "acknowledgeWarnings", message: "must be true or false." });
  }

  if (update.history !== undefined) {
    if (!isPlainObject(update.history)) {
      errors.push({ field: "history", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.history)) {
        if (key !== "retentionDays") errors.push({ field: `history.${key}`, message: "isn't a known setting." });
      }
      const days = update.history.retentionDays;
      if (days !== undefined && !(Number.isInteger(days) && days >= 7 && days <= 3650)) {
        errors.push({ field: "history.retentionDays", message: "must be a whole number of days from 7 to 3650." });
      }
    }
  }
  if (update.riskFacts !== undefined) {
    if (!isPlainObject(update.riskFacts)) {
      errors.push({ field: "riskFacts", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.riskFacts)) {
        if (key !== "url") errors.push({ field: `riskFacts.${key}`, message: "isn't a known setting." });
      }
      const url = update.riskFacts.url;
      if (url !== undefined) {
        const trimmed = typeof url === "string" ? url.trim() : null;
        if (trimmed === null || hasControlChars(trimmed) || trimmed.length > 500 || (trimmed && !isHttpUrl(trimmed))) {
          errors.push({ field: "riskFacts.url", message: "must be an http:// or https:// URL (or blank to turn it off)." });
        }
      }
    }
  }
  if (update.summarizeComments !== undefined) {
    if (!isPlainObject(update.summarizeComments)) {
      errors.push({ field: "summarizeComments", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.summarizeComments)) {
        if (key !== "model") errors.push({ field: `summarizeComments.${key}`, message: "isn't a known setting." });
      }
      if (update.summarizeComments.model !== undefined) {
        validateModel(errors, "summarizeComments.model", update.summarizeComments.model, { allowBlank: true });
      }
    }
  }

  if (update.addressReviewComments !== undefined) {
    if (!isPlainObject(update.addressReviewComments)) {
      errors.push({ field: "addressReviewComments", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.addressReviewComments)) {
        if (key !== "model") errors.push({ field: `addressReviewComments.${key}`, message: "isn't a known setting." });
      }
      if (update.addressReviewComments.model !== undefined) {
        validateModel(errors, "addressReviewComments.model", update.addressReviewComments.model, { allowBlank: true });
      }
    }
  }

  if (update.ticketToPr !== undefined) {
    if (!isPlainObject(update.ticketToPr)) {
      errors.push({ field: "ticketToPr", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.ticketToPr)) {
        if (key !== "reviewTransitionName" && key !== "autoMoveToReview") errors.push({ field: `ticketToPr.${key}`, message: "isn't a known setting." });
      }
      if (update.ticketToPr.autoMoveToReview !== undefined && typeof update.ticketToPr.autoMoveToReview !== "boolean") {
        errors.push({ field: "ticketToPr.autoMoveToReview", message: "must be true or false." });
      }
      const name = update.ticketToPr.reviewTransitionName;
      if (name !== undefined && (typeof name !== "string" || hasControlChars(name) || name.trim().length > 100)) {
        errors.push({
          field: "ticketToPr.reviewTransitionName",
          message: "must be a Jira transition or status name of at most 100 characters (blank for the default, In Review).",
        });
      }
    }
  }
  if (update.sessionCache !== undefined) {
    if (!isPlainObject(update.sessionCache)) {
      errors.push({ field: "sessionCache", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.sessionCache)) {
        if (key !== "ttlMinutes" && key !== "heartbeat") {
          errors.push({ field: `sessionCache.${key}`, message: "isn't a known setting." });
        }
      }
      if (update.sessionCache.ttlMinutes !== undefined) {
        const ttlMinutes = update.sessionCache.ttlMinutes;
        if (
          typeof ttlMinutes !== "number" ||
          !Number.isInteger(ttlMinutes) ||
          ttlMinutes < 0 ||
          ttlMinutes > MAX_SESSION_CACHE_TTL_MINUTES
        ) {
          errors.push({
            field: "sessionCache.ttlMinutes",
            message: `must be a whole number of minutes from 0 to ${MAX_SESSION_CACHE_TTL_MINUTES} (0 turns caching off).`,
          });
        }
      }
      if (update.sessionCache.heartbeat !== undefined && typeof update.sessionCache.heartbeat !== "boolean") {
        errors.push({ field: "sessionCache.heartbeat", message: "must be true or false." });
      }
    }
  }

  if (update.mcp !== undefined) {
    if (!isPlainObject(update.mcp)) {
      errors.push({ field: "mcp", message: "must be an object." });
    } else {
      for (const key of Object.keys(update.mcp)) {
        if (key !== "rotateToken") {
          errors.push({ field: `mcp.${key}`, message: "isn't a known setting." });
        }
      }
      if (update.mcp.rotateToken !== undefined && update.mcp.rotateToken !== true) {
        errors.push({ field: "mcp.rotateToken", message: "can only be true (leave it out to keep the token)." });
      }
    }
  }
  background.validateBackgroundSettings(update, current, errors);
  return errors;
}

/**
 * Applies a site update ({baseUrl, username?, apiToken?, clearApiToken?})
 * over `existing`: blank fields are removed (readers fall back to their
 * defaults / SSO), a blank or absent apiToken keeps the saved one.
 *
 * The token itself is never part of the returned object — and, per Fix
 * round 1, it's no longer written to the credential store from in here
 * either. mergeSite/mergeSettings stay pure: any token change is pushed
 * onto `tokenOps` (`{name, value}`, value `null` meaning "clear") for the
 * caller to apply once it's safe to — see mergeSettings' own doc comment
 * for why that matters.
 *
 * A legacy `apiToken` still in config.json (migrateTokensToStore failed
 * for it, e.g. an unwritable store) is the ONLY copy of that token, so it
 * is not simply dropped. With a new token or a clear in this update, that
 * wins and the legacy field goes. Otherwise, when `savedTokenNames` says
 * the store already has this token, the store's copy is authoritative and
 * the legacy field goes; when the store lacks it, a tokenOp moves the
 * legacy value into the store and the field goes with it; and when
 * `savedTokenNames` wasn't given (can't tell), the field is kept as-is.
 */
function mergeSite(existing, update, tokenName, tokenOps, savedTokenNames) {
  const next = { ...(existing || {}) };
  const legacyToken = typeof next.apiToken === "string" ? next.apiToken.trim() : "";
  delete next.apiToken;
  for (const key of ["baseUrl", "username"]) {
    if (update[key] === undefined) continue;
    let value = update[key].trim();
    if (key === "baseUrl") value = value.replace(/\/+$/, "");
    if (value) next[key] = value;
    else delete next[key];
  }
  if (update.clearApiToken) tokenOps.push({ name: tokenName, value: null });
  else if (typeof update.apiToken === "string" && update.apiToken.trim()) {
    tokenOps.push({ name: tokenName, value: update.apiToken.trim() });
  } else if (legacyToken) {
    if (!Array.isArray(savedTokenNames)) next.apiToken = existing.apiToken;
    else if (!savedTokenNames.includes(tokenName)) tokenOps.push({ name: tokenName, value: legacyToken });
  }
  return Object.keys(next).length > 0 ? next : undefined;
}

/**
 * The config to write for an already-validated update: a copy of `current`
 * with only the settings the panel owns replaced, plus `tokenOps` — the
 * token writes/clears (from mergeSite) that update implies, NOT yet
 * applied to any store.
 *
 * Fix round 1: this used to call `credentials.setToken` itself, from
 * inside mergeSite, which meant a token was saved or cleared before
 * server.ts's PUT /settings handler had checked the newly-enabled
 * features' prerequisite warnings (409, "acknowledge and retry") or
 * called writeConfig (which can fail, 500) — so cancelling that dialog,
 * or a config.json write failure, still silently changed the saved
 * token. Returning `{ config, tokenOps }` instead lets the caller apply
 * tokenOps only after both of those have succeeded, so a rejected or
 * failed save changes nothing at all, token included. Unknown fields,
 * port and sharedSecret pass through `config` untouched.
 *
 * `savedTokenNames` (the credential store's token names, e.g. from
 * credentials.listTokenNames) only decides what happens to a legacy
 * config.json token — see mergeSite. Passed in rather than looked up so
 * this stays pure.
 *
 * `newMcpToken` (in production, core/mcp-auth.js's generateMcpToken) is
 * called only for `mcp.rotateToken: true`, and its value only ever goes
 * into the returned mcp.token tokenOp — injected, like savedTokenNames,
 * so this stays pure and a test can pin the value.
 */
function mergeSettings(current, update, { savedTokenNames, newMcpToken } = {}) {
  const next = JSON.parse(JSON.stringify(current));
  const tokenOps = [];
  if (update.enabledFeatures !== undefined) {
    const chosen = new Set(update.enabledFeatures);
    next.enabledFeatures = registry.allFeatureIds().filter((id) => chosen.has(id));
    // Same snapshot setup writes, so a later update only auto-enables
    // features shipped after this save — not ones just unchecked here.
    next.knownFeatures = registry.allFeatureIds();
  }
  if (update.repos !== undefined) {
    next.repos = Object.fromEntries(
      Object.entries(update.repos).map(([key, clonePath]) => [key, path.resolve(clonePath.trim())]),
    );
  }
  if (update.reviewEditor !== undefined) {
    if (update.reviewEditor) next.reviewEditor = update.reviewEditor;
    else delete next.reviewEditor;
  }
  for (const key of environment.siteIds()) {
    if (update[key] === undefined) continue;
    const site = mergeSite(next[key], update[key], `${key}.apiToken`, tokenOps, savedTokenNames);
    if (site) next[key] = site;
    else delete next[key];
  }
  // The keys packs add to a site (e.g. jenkins.pipelines): saved only when they differ from the pack's default.
  for (const siteId of environment.siteIds()) {
    for (const extra of packs.siteSettingExtras(siteId)) {
      if (!update[siteId] || update[siteId][extra.key] === undefined) continue;
      const site = { ...(next[siteId] || {}) };
      const raw = update[siteId][extra.key];
      const checked = raw === null ? null : extra.validate(raw);
      const isDefault = checked && JSON.stringify(checked.value) === JSON.stringify(extra.defaultValue);
      if (checked && checked.ok && !isDefault) site[extra.key] = checked.value;
      else delete site[extra.key];
      if (Object.keys(site).length > 0) next[siteId] = site;
      else delete next[siteId];
    }
  }
  if (update.analyzeIssue !== undefined) {
    const analyzeIssue = { ...(next.analyzeIssue || {}) };
    if (update.analyzeIssue.model !== undefined) {
      const model = update.analyzeIssue.model.trim();
      if (model) analyzeIssue.model = model;
      else delete analyzeIssue.model;
    }
    for (const [key, validate, defaults] of [
      ["projects", targets.validateProjects, targets.DEFAULT_PROJECTS],
      ["issueTypes", targets.validateIssueTypes, targets.DEFAULT_ISSUE_TYPES],
      ["componentRepoMap", targets.validateComponentRepoMap, {}],
    ]) {
      if (update.analyzeIssue[key] === undefined) continue;
      const checked = update.analyzeIssue[key] === null ? null : validate(update.analyzeIssue[key]);
      const isDefault = checked && JSON.stringify(checked.value) === JSON.stringify(defaults);
      if (checked && checked.ok && !isDefault) analyzeIssue[key] = checked.value;
      else delete analyzeIssue[key];
    }
    if (Object.keys(analyzeIssue).length > 0) next.analyzeIssue = analyzeIssue;
    else delete next.analyzeIssue;
  }
  if (update.sessionCache !== undefined) {
    // Merge onto whatever's already saved (an update may set only one of
    // the two fields — see the "keeps the existing ttlMinutes" test), then
    // drop the key entirely once both fields are back at their defaults,
    // same convention as reviewEditor's "auto". ttlMinutes: 0 is
    // a real, meaningful value ("caching off"), never treated as unset.
    const merged = { ...(next.sessionCache || {}) };
    if (update.sessionCache.ttlMinutes !== undefined) merged.ttlMinutes = update.sessionCache.ttlMinutes;
    if (update.sessionCache.heartbeat !== undefined) merged.heartbeat = update.sessionCache.heartbeat;
    const isDefault =
      (merged.ttlMinutes ?? SESSION_CACHE_TTL_MINUTES_DEFAULT) === SESSION_CACHE_TTL_MINUTES_DEFAULT &&
      (merged.heartbeat ?? SESSION_CACHE_HEARTBEAT_DEFAULT) === SESSION_CACHE_HEARTBEAT_DEFAULT;
    if (isDefault) delete next.sessionCache;
    else next.sessionCache = merged;
  }
  if (update.history?.retentionDays !== undefined) {
    if (update.history.retentionDays === historySchema.DEFAULT_RETENTION_DAYS) delete next.history;
    else next.history = { retentionDays: update.history.retentionDays };
  }
  if (update.riskFacts?.url !== undefined) {
    const url = update.riskFacts.url.trim();
    if (url) next.riskFacts = { url };
    else delete next.riskFacts;
  }
  if (update.summarizeComments?.model !== undefined) {
    const model = update.summarizeComments.model.trim();
    if (model) next.summarizeComments = { model };
    else delete next.summarizeComments;
  }
  if (update.addressReviewComments?.model !== undefined) {
    // checkCommands (config.json only) rides along untouched.
    const addressReviewComments = { ...(next.addressReviewComments || {}) };
    const model = update.addressReviewComments.model.trim();
    if (model) addressReviewComments.model = model;
    else delete addressReviewComments.model;
    if (Object.keys(addressReviewComments).length > 0) next.addressReviewComments = addressReviewComments;
    else delete next.addressReviewComments;
  }
  if (update.ticketToPr?.reviewTransitionName !== undefined || update.ticketToPr?.autoMoveToReview !== undefined) {
    // The defaults (In Review, automatic) aren't persisted, like reviewEditor's "auto".
    const saved = next.ticketToPr || {};
    const name = update.ticketToPr.reviewTransitionName !== undefined ? update.ticketToPr.reviewTransitionName.trim() : saved.reviewTransitionName;
    const auto = update.ticketToPr.autoMoveToReview !== undefined ? update.ticketToPr.autoMoveToReview : saved.autoMoveToReview !== false;
    const out = {};
    if (name && name !== TICKET_TO_PR_TRANSITION_DEFAULT) out.reviewTransitionName = name;
    if (auto === false) out.autoMoveToReview = false;
    if (Object.keys(out).length > 0) next.ticketToPr = out;
    else delete next.ticketToPr;
  }
  if (update.mcp?.rotateToken === true) {
    if (typeof newMcpToken !== "function") throw new Error("mergeSettings needs newMcpToken to rotate the MCP token.");
    tokenOps.push({ name: MCP_TOKEN_NAME, value: newMcpToken() });
  }
  background.mergeBackgroundSettings(next, update, tokenOps);
  return { config: next, tokenOps };
}

/**
 * Applies mergeSettings' `tokenOps` in order via `setToken(name, value)`,
 * stopping at the first failure rather than trying the rest. Each
 * `setToken` call is a complete, synchronous write (see core/token-
 * cache.js) — there's no "half applied" for a single op to roll back —
 * so any ops before the failing one stay applied exactly as they were;
 * this only reports where things stopped, it never undoes anything.
 *
 * Returns `{ ok: true }` on full success, or `{ ok: false, failedOp,
 * error }` naming the op that threw and the error, so a caller (Fix
 * round 2: server.ts's PUT /settings) can report a specific, actionable
 * failure without needing its own try/catch around the loop. `setToken`
 * is injected — in production, core/credentials.ts's `setToken`; a fake
 * in tests, never the real store.
 */
function applyTokenOps(tokenOps, setToken) {
  for (const op of tokenOps) {
    try {
      setToken(op.name, op.value);
    } catch (err) {
      return { ok: false, failedOp: op, error: err };
    }
  }
  return { ok: true };
}

/**
 * Prerequisite checks failing for any of `ids`, as
 * [{ check, message, features: [label] }] — what setup's "Needed by … press
 * Enter to check again, or skip" step shows, for the panel to show before
 * saving. `checks` is injectable for tests.
 */
function prerequisiteWarnings(ids, checks = prereqs.CHECKS_BY_NAME) {
  const warnings = [];
  for (const name of registry.requiredChecksFor(ids)) {
    const result = checks[name]();
    if (result.ok) continue;
    warnings.push({
      check: name,
      message: result.message,
      features: registry.featuresRequiring(name, ids).map((d) => d.label),
    });
  }
  return warnings;
}

/** Results of every check any feature can require, keyed by check name —
 * lets the panel show whether a feature is ready before it's enabled. */
function runAllChecks(checks = prereqs.CHECKS_BY_NAME) {
  const names = registry.requiredChecksFor(registry.allFeatureIds());
  return Object.fromEntries(names.map((name) => [name, checks[name]()]));
}

/**
 * Whether the saved settings differ from what the running service loaded.
 * Only the enabled feature set needs a restart: server.ts builds each
 * feature and its routes once, at startup. Every other setting the panel
 * edits is read from the shared config object whenever a job runs.
 */
function pendingRestart(runningEnabledIds, savedConfig) {
  const saved = registry.enabledFeatureIds(savedConfig);
  const added = saved.filter((id) => !runningEnabledIds.includes(id));
  const removed = runningEnabledIds.filter((id) => !saved.includes(id));
  return { required: added.length > 0 || removed.length > 0, added, removed };
}

/** Local clones (from prereqs.discoverLocalClones) not already mapped by
 * key or by folder — offered in the panel as one-click additions. */
function suggestedRepos(existingRepos, clones) {
  const keys = new Set(Object.keys(existingRepos).map((k) => k.toLowerCase()));
  const paths = new Set(Object.values(existingRepos).map((p) => path.resolve(p)));
  return clones.filter((c) => !keys.has(c.key.toLowerCase()) && !paths.has(path.resolve(c.path)));
}

/**
 * Writes config.json via a temp file + rename in the same folder, so a
 * crash or a concurrent reader never sees half a file. Keeps the existing
 * file's permissions (it holds the shared secret and any API tokens);
 * a new file is created owner-only.
 */
function writeConfigFile(filePath, config) {
  let mode = 0o600;
  try {
    mode = fs.statSync(filePath).mode & 0o777;
  } catch {
    // No file yet — owner-only default.
  }
  const tmp = path.join(path.dirname(filePath), `.${path.basename(filePath)}.${process.pid}.${Date.now()}.tmp`);
  try {
    fs.writeFileSync(tmp, JSON.stringify(config, null, 2) + "\n", { mode });
    fs.renameSync(tmp, filePath);
  } catch (err) {
    fs.rmSync(tmp, { force: true });
    throw err;
  }
}

// Which config.json fields Task 3 moved into the encrypted credential
// store, and the store name each one moved to. Deliberately only these
// three (not mcp.token/slack.token, which credential-store.js's
// ALLOWED_NAMES also covers) — those never lived in config.json, so
// there's nothing of theirs to migrate. `bitbucket` is handled on the same
// footing as jira/jenkins (Task 5 added it to config.ts's Config type and
// to settings.js's EDITABLE_KEYS/publicSettings/mergeSettings above).
const TOKEN_MIGRATIONS = environment.siteIds().map((site) => ({ site, name: `${site}.apiToken` }));

/**
 * One-time move of any `apiToken` still sitting in config.json (jira/
 * jenkins/bitbucket only) into the encrypted credential store. Runs at
 * service startup (after loadConfig) and in setup.js, both of which build
 * `config` from scratch each time — so idempotent is what makes repeat
 * runs harmless: nothing left to migrate means nothing is written to the
 * store and `saveConfig` is never called (a second run against an
 * already-blank config is a complete no-op).
 *
 * Order matters for safety, not just tidiness: every non-empty token is
 * set in the store FIRST; only once every one of those `store.set` calls
 * has succeeded are the config fields deleted and `saveConfig` called. A
 * mid-way store failure (a bad key, a full disk) therefore leaves
 * config.json exactly as it was — the token(s) stay right where they
 * are, still readable by the SSO-first fallback chain, not silently
 * dropped — and is reported with a single WARN rather than one per
 * token. `store` is `{ set }` (credential-store.js's real shape, or a
 * fake in tests); `saveConfig` is injected so this stays pure-ish and
 * testable rather than reaching for a real config.json write itself.
 */
function migrateTokensToStore(config, store, saveConfig) {
  const toMigrate = TOKEN_MIGRATIONS.filter(({ site }) => {
    const value = config[site] && config[site].apiToken;
    return typeof value === "string" && value.trim() !== "";
  });
  if (toMigrate.length === 0) return { migrated: [] };

  try {
    for (const { site, name } of toMigrate) {
      store.set(name, config[site].apiToken);
    }
  } catch (err) {
    console.warn(
      `[credentials] Could not move ${toMigrate.map((t) => t.name).join(", ")} into the encrypted ` +
        `credential store (${err.message}); left as-is in config.json for now — this will be retried ` +
        "on the next startup.",
    );
    return { migrated: [] };
  }

  for (const { site } of toMigrate) delete config[site].apiToken;
  saveConfig(config);
  return { migrated: toMigrate.map((t) => t.name) };
}

module.exports = {
  LOCKED_KEYS,
  publicSettings,
  featureSummaries,
  validateSettingsUpdate,
  mergeSettings,
  applyTokenOps,
  prerequisiteWarnings,
  runAllChecks,
  pendingRestart,
  suggestedRepos,
  writeConfigFile,
  migrateTokensToStore,
};
