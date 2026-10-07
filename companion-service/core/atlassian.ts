// Shared "how does a feature talk to an internal Atlassian site" helper.
// The companion service has no browser session of its own — see
// background.js's relayHeadersFor for how the extension gets one to it —
// so every feature that needs Jira/Confluence goes through here rather
// than reimplementing this fallback chain itself.
//
// Auth order, per the user's explicit instruction to try SSO first:
//   1. The browser session relayed from the page (X-Relay-Cookie /
//      X-Relay-Origin, set by background.js). Only ever forwarded to the
//      exact origin it came from — see the origin check below.
//   2. A configured API token — `site.apiToken`, which callers resolve via
//      core/credentials.ts's getToken("jira.apiToken") rather than reading
//      config.json directly (Task 3: tokens live in the encrypted
//      companion-service/credentials.enc now, not config.json).
//   3. Neither works (or no token is saved) -> AuthSetupError, whose
//      message is written to be shown directly to the user (it ends up in
//      job.error, which content.js already renders in the review panel)
//      pointing them at ✨ → ⚙ Settings.
//
// Task 2 added a middle step between 1 and 2: a *cached* browser session
// from some other request, or from the extension's heartbeat — see
// core/auth-context.ts's authContextFor, called at the top of
// authedJsonWithHeaders below.
import { authContextFor } from "./auth-context";

export interface AuthContext {
  /** "name=value; name2=value2" relayed from the page's own cookies. */
  cookie?: string;
  /** Origin the cookie was relayed from — the cookie is only ever used
   * against a site whose baseUrl has this same origin. */
  origin?: string;
}

export interface SiteAuth {
  /** e.g. "https://jira.example.com" — no trailing slash. */
  baseUrl: string;
  /** Human-readable name for error messages, e.g. "Jira". */
  label: string;
  /** Dotted config path for error messages, e.g. "jira.apiToken". */
  configKey: string;
  username?: string;
  apiToken?: string;
  /**
   * How to present `apiToken` on the fallback attempt. Defaults to
   * "bearer" (Jira Server's scheme, and what this file did before any
   * other site existed). "basic" sends `username:apiToken` base64-encoded
   * — Jenkins' convention, where the "password" half is an API token
   * rather than a real password. See core/jenkins.ts.
   */
  authScheme?: "bearer" | "basic";
  /**
   * The site has no browser login the companion can borrow (GitHub's page cookies don't authenticate its
   * API): a saved token is the only way in, so the session cookie and vault are never tried and a 403 is
   * read as "not allowed" or "rate limited", not as "bad token".
   */
  tokenOnly?: boolean;
  /** Extra request headers for every call (e.g. GitHub's Accept and API version); they win over the defaults. */
  headers?: Record<string, string>;
}

export class AuthSetupError extends Error {}

/** A non-ok HTTP response, carrying the status so callers can react to a
 * specific one. core/jenkins.ts needs this: Jenkins answers 404 for a
 * build that published no test report, which is ordinary "nothing here"
 * rather than an error worth failing the job over. */
export class HttpStatusError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

function isRedirectOrAuthFailure(res: Response): boolean {
  if (res.status === 401 || res.status === 403) return true;
  if (res.status >= 300 && res.status < 400) return true;
  // Jira Server's reliable "you're not actually logged in" signal even on
  // a 200 (some endpoints don't bother with a 401) — confirmed against
  // this org's Jira instance.
  if ((res.headers.get("x-ausername") || "").toLowerCase() === "anonymous") return true;
  return false;
}

/** The Authorization header for `site`'s configured token — Bearer unless
 * the site asked for HTTP Basic (see SiteAuth.authScheme). Basic needs a
 * username to pair the token with, so a half-configured site fails here
 * with the same "go fix your config" wording as a missing token. */
function authorizationHeader(site: SiteAuth): string {
  if (site.authScheme !== "basic") return `Bearer ${site.apiToken}`;
  if (!site.username) {
    throw new AuthSetupError(
      `${site.label} needs a username alongside its API token. Add the username next to ` +
        `"${site.configKey}" in companion-service/config.json (see config.example.json) and restart the service.`,
    );
  }
  const encoded = Buffer.from(`${site.username}:${site.apiToken}`).toString("base64");
  return `Basic ${encoded}`;
}

/** authedJson's result, plus the response headers of whichever attempt
 * actually answered (cookie or token) — needed by callers that read more
 * than the body, e.g. core/bitbucket.ts's whoAmI reading X-AUSERNAME. */
export interface AuthedResponse {
  body: unknown;
  headers: Headers;
}

/**
 * Fetch JSON from `site.baseUrl + pathAndQuery`, trying the relayed browser
 * session first and falling back to an API token. Throws AuthSetupError
 * (safe to surface to the user as-is) if neither works. Returns both the
 * parsed body and the response headers — see AuthedResponse.
 */
export async function authedJsonWithHeaders(
  site: SiteAuth,
  auth: AuthContext,
  pathAndQuery: string,
  init: { method?: string; body?: unknown } = {},
): Promise<AuthedResponse> {
  // Task 2: try the cached browser session (recorded from some *other*
  // request, or the extension's heartbeat) before falling back to
  // site.apiToken — see core/auth-context.ts. `site`/`auth` are
  // reassigned in place so the rest of this function, unchanged, keeps
  // working with whichever cookie/token actually won.
  const resolved = site.tokenOnly ? { site, auth, source: "none" as const } : authContextFor(site, auth);
  site = resolved.site;
  auth = resolved.auth;

  const url = `${site.baseUrl}${pathAndQuery}`;
  const baseHeaders: Record<string, string> = { Accept: "application/json", ...site.headers };
  const body = init.body !== undefined ? JSON.stringify(init.body) : undefined;
  if (body !== undefined) baseHeaders["Content-Type"] = "application/json";

  const cookieUsable = auth.cookie && auth.origin === new URL(site.baseUrl).origin;
  if (cookieUsable) {
    const res = await fetch(url, {
      method: init.method || "GET",
      headers: { ...baseHeaders, Cookie: auth.cookie! },
      body,
      // A login redirect must not be silently followed into an HTML page
      // and mistaken for a real (empty-ish) JSON response.
      redirect: "manual",
    });
    if (!isRedirectOrAuthFailure(res)) {
      return { body: await parseJsonOrThrow(res, site), headers: res.headers };
    }
    // Falls through to the token attempt below.
  }

  if (site.apiToken) {
    const res = await fetch(url, {
      method: init.method || "GET",
      headers: { ...baseHeaders, Authorization: authorizationHeader(site) },
      body,
    });
    if (site.tokenOnly) {
      if (res.status === 401) {
        throw new AuthSetupError(
          `${site.label} rejected the saved API token (${site.configKey}). ` +
            `Generate a fresh token and save it in ✨ → ⚙ Settings.`,
        );
      }
      // A 403/429 on a token that works is a rate limit or a missing permission, not a bad token.
      if ((res.status === 403 || res.status === 429) && rateLimited(res)) {
        throw new HttpStatusError(`${site.label} rate limit reached${rateLimitReset(res)}. Try again later.`, res.status);
      }
      return { body: await parseJsonOrThrow(res, site), headers: res.headers };
    }
    if (isRedirectOrAuthFailure(res)) {
      throw new AuthSetupError(
        `${site.label} rejected the saved API token (${site.configKey}). ` +
          `Generate a fresh token and save it in ✨ → ⚙ Settings.`,
      );
    }
    return { body: await parseJsonOrThrow(res, site), headers: res.headers };
  }

  if (site.tokenOnly) {
    throw new AuthSetupError(
      `No ${site.label} API token is saved, and ${site.label} can only be used with one. ` +
        `Create a personal access token and add it in ✨ → ⚙ Settings.`,
    );
  }

  if (resolved.source === "none") {
    // There was no browser session (request or vault) to even try, so
    // "rejected" would be misleading — a different, more actionable
    // message than the one below (which is for a cookie that *was* tried
    // and turned out to be stale/invalid).
    throw new AuthSetupError(
      `No ${site.label} browser session is available to the companion right now (open ${site.label} in ` +
        `Chrome, or turn on "Keep browser sessions warm" in ✨ → ⚙ Settings), and no API token is saved. ` +
        `Add one in ✨ → ⚙ Settings.`,
    );
  }

  throw new AuthSetupError(
    `${site.label} rejected the browser session relayed from the extension, and no API token is ` +
      `saved. Add one in ✨ → ⚙ Settings — or make sure you're logged into ${site.label} in this browser.`,
  );
}

/**
 * Fetch JSON from `site.baseUrl + pathAndQuery` — see
 * authedJsonWithHeaders for the actual auth fallback logic, which this
 * shares completely (no duplication); this is just its body half, for the
 * majority of callers that never need the response headers.
 */
export async function authedJson(
  site: SiteAuth,
  auth: AuthContext,
  pathAndQuery: string,
  init: { method?: string; body?: unknown } = {},
): Promise<unknown> {
  return (await authedJsonWithHeaders(site, auth, pathAndQuery, init)).body;
}

/** Whether a 403/429 is the host telling us to slow down (GitHub: no calls left, or a retry-after for secondary limits). */
function rateLimited(res: Response): boolean {
  return res.status === 429 || res.headers.get("x-ratelimit-remaining") === "0" || res.headers.has("retry-after");
}

function rateLimitReset(res: Response): string {
  const reset = Number(res.headers.get("x-ratelimit-reset"));
  const retryAfter = Number(res.headers.get("retry-after"));
  if (retryAfter > 0) return ` (retry in ${retryAfter}s)`;
  return reset > 0 ? ` (resets at ${new Date(reset * 1000).toISOString().slice(11, 16)} UTC)` : "";
}

async function parseJsonOrThrow(res: Response, site: SiteAuth): Promise<unknown> {
  const text = await res.text();
  let data: unknown = undefined;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    // Falls through to the ok/not-ok handling below with data left undefined.
  }
  if (!res.ok) {
    const message =
      (data && typeof data === "object" && "errorMessages" in (data as Record<string, unknown>)
        ? (data as { errorMessages?: string[] }).errorMessages?.join("; ")
        : undefined) ||
      // GitHub's error body: { message, documentation_url }.
      (data && typeof data === "object" && typeof (data as { message?: unknown }).message === "string"
        ? (data as { message: string }).message
        : undefined) ||
      text ||
      `HTTP ${res.status}`;
    throw new HttpStatusError(`${site.label} request failed: ${message}`, res.status);
  }
  return data;
}
