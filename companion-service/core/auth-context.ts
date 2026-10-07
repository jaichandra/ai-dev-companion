// Wires core/session-vault.js's in-memory cache of relayed browser
// sessions into every feature's auth path: authContextFor tries the
// request's own relayed cookie, then the vault (a session relayed by some
// *other* request, or by the extension's heartbeat — see heartbeatHosts),
// then leaves the token fallback to core/atlassian.ts's authedJsonWithHeaders
// exactly as before. Plain TypeScript, not JS, because it needs the
// AuthContext/SiteAuth types from atlassian.ts and Config from config.ts —
// same reasoning as core/jenkins.ts / core/bitbucket.ts.
import { AuthContext, SiteAuth } from "./atlassian";
import { Config } from "../config";
// eslint-disable-next-line @typescript-eslint/no-var-requires
const environment = require("../environment.js") as { sites: { id: string; tokenOnly?: boolean }[]; siteIds(): string[]; defaultBaseUrl(id: string): string };

interface SessionVault {
  record(origin: string, cookie: string): boolean;
  get(origin: string): string | undefined;
  clear(): void;
  size(): number;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const vaultModule = require("./session-vault.js") as {
  originOf(baseUrl: string): string | null;
  createSessionVault(opts: { ttlMs: () => number; allowedOrigins: () => string[] }): SessionVault;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./credentials.js") as { getToken(name: string): string | undefined };

/**
 * `null` until configureSessionVault runs. doctor, setup and every test
 * import this module without ever calling configureSessionVault, so they
 * never cache a cookie — authContextFor and recordRelayedSession both
 * check for `null` and become no-ops.
 */
let vault: SessionVault | null = null;

/**
 * Builds the singleton vault, called once from server.ts right after
 * FEATURES is built. `ttlMs`/`allowedOrigins` read `config` live (the same
 * mutable object applySavedConfig updates in place elsewhere in server.ts)
 * rather than snapshotting values at startup, so a sessionCache change
 * saved from the Settings panel takes effect on the very next request —
 * no restart, matching settings.js's pendingRestart, which doesn't flag
 * sessionCache as needing one. Safe to call more than once (there's
 * nothing to lose — this only ever runs once, before any cookie could
 * have been cached).
 */
export function configureSessionVault(config: Config): void {
  vault = vaultModule.createSessionVault({
    ttlMs: () => sessionTtlMs(config),
    allowedOrigins: () => configuredOrigins(config),
  });
}

/**
 * The origins any Jira/Jenkins/Bitbucket call could need a relayed cookie
 * for — the only origins the vault will ever accept a cookie against
 * (session-vault.js's record() checks this exact list, since
 * X-Relay-Origin comes from the client and isn't trusted otherwise).
 * De-duplicated: nothing stops two of the three sites sharing a host.
 */
export function configuredOrigins(config: Config): string[] {
  const configured = config as unknown as Record<string, { baseUrl?: string } | undefined>;
  // A token-only site (GitHub) has no browser session worth relaying, so its origin is never accepted for one.
  const ids = environment.sites.filter((s) => !s.tokenOnly).map((s) => s.id);
  const origins = ids.map((id) => vaultModule.originOf(configured[id]?.baseUrl || environment.defaultBaseUrl(id))).filter((origin): origin is string => !!origin);
  return [...new Set(origins)];
}

/** How long a cached cookie stays usable. 0 (or below) turns caching off —
 * session-vault.js's record()/get() both already treat `ttlMs() <= 0` as
 * "never cache anything", so there's nothing extra to gate here. */
export function sessionTtlMs(config: Config): number {
  return (config.sessionCache?.ttlMinutes ?? 30) * 60_000;
}

/**
 * Caches a relayed browser session so a later request that carries no
 * cookie of its own — an MCP tool call from terminal Claude, or a feature
 * call for a site the current page isn't open to — can still act as the
 * logged-in user for a while. A no-op until configureSessionVault has run
 * (doctor/setup/tests never call it), or when `auth` is missing either
 * field. The vault's own record() additionally refuses any origin that
 * isn't one of configuredOrigins — see its comment.
 */
export function recordRelayedSession(auth: AuthContext): void {
  if (!vault || !auth.cookie || !auth.origin) return;
  vault.record(auth.origin, auth.cookie);
}

/**
 * Resolves the auth to actually use for `site`, trying in order: the
 * request's own relayed cookie (only if it matches site's origin) -> the
 * vault's cached cookie for that origin -> nothing (core/atlassian.ts's
 * authedJsonWithHeaders falls back to site.apiToken from there, exactly as
 * it always has). `site.apiToken` is filled in from
 * credentials.getToken(site.configKey) only when the caller left it
 * `undefined` — jenkins.ts and bitbucket.ts already resolve it themselves
 * and keep doing so; this only helps callers that don't.
 */
export function authContextFor(
  site: SiteAuth,
  auth: AuthContext = {},
): { auth: AuthContext; site: SiteAuth; source: "request" | "vault" | "none" } {
  const origin = vaultModule.originOf(site.baseUrl);
  const withToken: SiteAuth =
    site.apiToken !== undefined ? site : { ...site, apiToken: credentials.getToken(site.configKey) };
  if (!origin) return { auth: {}, site: withToken, source: "none" };
  const fromRequest = auth.cookie && auth.origin === origin ? auth.cookie : undefined;
  const cookie = fromRequest ?? (vault ? vault.get(origin) : undefined);
  return {
    auth: cookie ? { cookie, origin } : {},
    site: withToken,
    source: fromRequest ? "request" : cookie ? "vault" : "none",
  };
}

/**
 * Which origins the extension should proactively keep a fresh cookie
 * relayed for, even without a matching feature call — server.ts's
 * GET /extension/session-hosts. Only when the panel turned heartbeat on
 * AND the cache is actually able to hold anything (ttl > 0); a heartbeat
 * feeding a vault that's off would just relay cookies over the wire for
 * nothing to keep.
 */
export function heartbeatHosts(config: Config): { heartbeat: boolean; hosts: string[] } {
  const on = config.sessionCache?.heartbeat === true && sessionTtlMs(config) > 0;
  return { heartbeat: on, hosts: on ? configuredOrigins(config) : [] };
}
