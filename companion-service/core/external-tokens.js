// Fallback tokens another tool on this machine already holds (a pack names where to look), so the companion works without a second copy of the same
// Jira / Bitbucket tokens. Where to look comes from the packs. Read at call time
// (cached for a minute), never copied into credentials.enc, and used only when
// the companion has no saved token of its own. A token is handed out only for
// the host its own config names: the origin a source reports must equal the
// configured site's, so it can never be sent anywhere else.
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const CACHE_MS = 60_000;

function parseEnvFile(text) {
  const out = {};
  for (const line of String(text).split(/\r?\n/)) {
    const m = /^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$/.exec(line);
    if (!m) continue;
    out[m[1]] = m[2].replace(/^(["'])(.*)\1$/, "$2");
  }
  return out;
}

function originOf(url) {
  try {
    const u = new URL(url);
    return u.protocol === "https:" || u.protocol === "http:" ? u.origin : null;
  } catch {
    return null;
  }
}

function createExternalTokens({ home = os.homedir(), fsImpl = fs, clock = Date.now, sources = () => require("./packs.js").externalTokenSources() } = {}) {
  let cached = null;
  let cachedAt = -Infinity;

  function readJson(file) {
    try {
      return JSON.parse(fsImpl.readFileSync(file, "utf8"));
    } catch {
      return null;
    }
  }

  function readText(file) {
    try {
      return fsImpl.readFileSync(file, "utf8");
    } catch {
      return null;
    }
  }

  // Each source reads one place a tool keeps a token and returns the entries it
  // found; where to look is the packs' business (core/packs.js `externalTokens`).
  function load() {
    const found = {};
    for (const source of sources()) {
      try {
        Object.assign(found, source({ home, fsImpl, readText, readJson, parseEnvFile, originOf }));
      } catch {
        /* one unreadable source never hides the others */
      }
    }
    return found;
  }

  /** `{ token, source }` (plus `username` for Jenkins' HTTP Basic) for `name` ("jira.apiToken" | "bitbucket.apiToken" | "jenkins.apiToken") when its origin is `baseUrl`'s. */
  function tokenFor(name, baseUrl) {
    const now = clock();
    if (!cached || now - cachedAt >= CACHE_MS) {
      cached = load();
      cachedAt = now;
    }
    const entry = cached[name];
    const wanted = originOf(baseUrl);
    if (!entry || !entry.origin || !wanted || entry.origin !== wanted) return undefined;
    return entry.username ? { token: entry.token, username: entry.username, source: entry.source } : { token: entry.token, source: entry.source };
  }

  /** Which file each site's fallback token would come from, for status lines: `{ jira?, jenkins?, bitbucket? }`. Never the token. */
  function sourcesFor(baseUrls) {
    const out = {};
    for (const site of require("../environment.js").siteIds()) {
      const hit = baseUrls && baseUrls[site] ? tokenFor(`${site}.apiToken`, baseUrls[site]) : undefined;
      if (hit) out[site] = hit.source;
    }
    return out;
  }

  return { tokenFor, sourcesFor };
}

module.exports = { createExternalTokens, parseEnvFile, originOf };
