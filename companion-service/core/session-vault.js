// In-memory cache of the browser sessions the extension relays
// (X-Relay-Cookie / X-Relay-Origin), so a request that carries no cookie
// of its own — an MCP tool call from terminal Claude — can still act as
// the logged-in user for a while. Plain JS for node --test. Never logged,
// never written anywhere: the Map below is the only copy.
const util = require("util");

const MAX_COOKIE_LENGTH = 16 * 1024;

function originOf(baseUrl) {
  try {
    const url = new URL(baseUrl);
    return url.protocol === "http:" || url.protocol === "https:" ? url.origin : null;
  } catch {
    return null;
  }
}

function isUsableCookie(cookie) {
  return typeof cookie === "string" && cookie.length > 0 && cookie.length <= MAX_COOKIE_LENGTH && !/[\r\n]/.test(cookie);
}

function createSessionVault({ ttlMs, allowedOrigins, clock = Date.now }) {
  const entries = new Map();
  const vault = {
    record(origin, cookie) {
      if (ttlMs() <= 0) return false;
      if (typeof origin !== "string" || !origin || !isUsableCookie(cookie)) return false;
      // Exact origin match only — X-Relay-Origin is client-supplied.
      if (!allowedOrigins().includes(origin)) return false;
      entries.set(origin, { cookie, at: clock() });
      return true;
    },
    get(origin) {
      const entry = entries.get(origin);
      if (!entry) return undefined;
      const ttl = ttlMs();
      if (ttl <= 0 || clock() - entry.at > ttl || !allowedOrigins().includes(origin)) {
        entries.delete(origin);
        return undefined;
      }
      return entry.cookie;
    },
    clear() {
      entries.clear();
    },
    size() {
      return entries.size;
    },
    toJSON() {
      return { entries: entries.size };
    },
    [util.inspect.custom]() {
      return `SessionVault { entries: ${entries.size} }`;
    },
  };
  return vault;
}

function pickCookie(siteOrigin, requestAuth, vault) {
  if (requestAuth && requestAuth.cookie && requestAuth.origin === siteOrigin) return requestAuth.cookie;
  return vault.get(siteOrigin);
}

module.exports = { MAX_COOKIE_LENGTH, originOf, createSessionVault, pickCookie };
