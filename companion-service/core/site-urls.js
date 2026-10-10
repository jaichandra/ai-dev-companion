// Setup's "your servers" step: a profile whose default base URL is a placeholder (the framework's own
// example.com hosts) can't work as shipped, so setup asks for the real address. A profile with real hosts
// (every distribution) is never asked, and neither is a site whose address config.json already holds.
// Plain JS, no dependencies: setup.js requires it without a build.

/** Whether `url` points at an example.com host, i.e. a profile placeholder rather than a real server. */
function isPlaceholderUrl(url) {
  try {
    const host = new URL(url).hostname.toLowerCase();
    return host === "example.com" || host.endsWith(".example.com");
  } catch {
    return false;
  }
}

/** The profile's sites that still need an address: placeholder default and nothing saved in `config`. */
function sitesNeedingUrl(environment, config = {}) {
  return environment.sites.filter((site) => isPlaceholderUrl(site.baseUrl) && !(config[site.id] && config[site.id].baseUrl));
}

/** Normalizes a typed address to an origin-style URL without a trailing slash, or "" when it isn't http(s). */
function normalizeUrl(text) {
  const trimmed = String(text || "").trim();
  if (!trimmed) return "";
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
  try {
    const url = new URL(withScheme);
    if (url.protocol !== "http:" && url.protocol !== "https:") return "";
    return withScheme.replace(/\/+$/, "");
  } catch {
    return "";
  }
}

/**
 * Asks for the address of each site in `sitesNeedingUrl`. Enter skips a site (it stays unconfigured and
 * Settings can set it later). Returns the config fragment to merge: `{ [siteId]: { ...existing, baseUrl } }`.
 * `ask(question, default)` is setup's prompt helper; `log` prints a line.
 */
async function promptSiteUrls(environment, config, { ask, log = console.log }) {
  const pending = sitesNeedingUrl(environment, config);
  const out = {};
  if (pending.length === 0) return out;
  log("This build ships with placeholder server addresses. Enter yours (Enter skips one; ⚙ Settings can set it later).");
  for (const site of pending) {
    for (;;) {
      const answer = await ask(`${site.label} address, e.g. ${site.baseUrl}`, "");
      if (!answer) break;
      const url = normalizeUrl(answer);
      if (url) {
        out[site.id] = { ...(config[site.id] || {}), baseUrl: url };
        break;
      }
      log("  That doesn't look like an http(s) address, try again or press Enter to skip.");
    }
  }
  return out;
}

module.exports = { isPlaceholderUrl, sitesNeedingUrl, normalizeUrl, promptSiteUrls };
