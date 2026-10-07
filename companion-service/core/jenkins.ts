// Shared "how does a feature talk to Jenkins" helper, mirroring
// core/atlassian.ts. Auth reuses atlassian.ts's authedJson wholesale —
// relayed browser session first, configured token as fallback,
// AuthSetupError otherwise — so there's exactly one copy of that fallback
// chain. The only Jenkins-specific part of auth is the scheme: Jenkins
// wants HTTP Basic with username + API token rather than a Bearer token
// (see SiteAuth.authScheme).
//
// Everything else here exists to keep the response sizes sane. Jenkins'
// REST API takes a `tree=` parameter selecting exactly the fields wanted,
// across *all* builds of a job in one request — which turns what would be
// a per-build walk into three calls, and, for test reports, is the
// difference between 0.8-1.8 MB of stack traces and stdout per build and a
// few tens of KB. Every endpoint below is therefore expressed as a tree
// projection, never a bare api/json.

import { AuthContext, AuthSetupError, HttpStatusError, SiteAuth, authedJson } from "./atlassian";
import { Config, DEFAULT_JENKINS_BASE_URL } from "../config";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./credentials.js") as { getToken(name: string): string | undefined };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const externalTokens = (require("./external-tokens.js") as { createExternalTokens(): { tokenFor(name: string, baseUrl: string): { token: string; username?: string } | undefined } }).createExternalTokens();

export interface JenkinsCause {
  upstreamProject?: string;
  upstreamBuild?: number;
}

export interface JenkinsParameter {
  name?: string;
  value?: unknown;
}

export interface JenkinsAction {
  causes?: JenkinsCause[];
  parameters?: JenkinsParameter[];
}

export interface JenkinsBuild {
  number: number;
  result: string | null;
  building: boolean;
  timestamp?: number;
  actions?: JenkinsAction[];
}

export interface JenkinsTestCase {
  className?: string;
  name?: string;
  status?: string;
  age?: number;
  failedSince?: number;
  errorDetails?: string;
  skipped?: boolean;
}

export interface JenkinsTestReport {
  failCount?: number;
  passCount?: number;
  skipCount?: number;
  suites?: { name?: string; cases?: JenkinsTestCase[] }[];
}

/** Trailing slashes off, and a scheme added if the configured value is
 * missing one. The setup wizard asks for the base URL as free text, and a
 * bare "jenkins.example.com" would otherwise make
 * `new URL()` throw inside every single request of a scan. */
function normalizeBaseUrl(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, "");
  return /^https?:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
}

/**
 * Jenkins as an authenticated site. `username` is required only for the
 * token fallback — a developer already logged into Blue Ocean in this
 * browser needs neither, since the extension relays that session.
 */
export function jenkinsSite(config: Config): SiteAuth {
  const baseUrl = normalizeBaseUrl(config.jenkins?.baseUrl || DEFAULT_JENKINS_BASE_URL);
  const saved = credentials.getToken("jenkins.apiToken");
  // Own saved token first (with the configured username); otherwise the
  // username and password another tool stored for this same host (core/external-tokens.js).
  const external = saved === undefined ? externalTokens.tokenFor("jenkins.apiToken", baseUrl) : undefined;
  return {
    baseUrl,
    label: "Jenkins",
    configKey: "jenkins.apiToken",
    username: config.jenkins?.username || external?.username || undefined,
    apiToken: saved ?? external?.token,
    authScheme: "basic",
  };
}

// ---- Caching ----
//
// A finished build's result never changes, so re-opening the panel should
// not re-download anything already seen. Only *finished* builds are
// cached; a still-building one is deliberately re-fetched every time.
// In-memory and unbounded-but-tiny, same reasoning as core/jobs.ts's
// JobStore — a service restart just re-fetches.

// Only ever holds real reports — a 404 is not cached, see fetchTestReport.
// Callers are responsible for not asking to cache a still-building build's
// report; every call site does that by filtering on `building` first.
const testReportCache = new Map<string, JenkinsTestReport>();

function cacheKey(job: string, buildNumber: number): string {
  return `${job}#${buildNumber}`;
}

/** Drops every cached response. Exported for tests and for a future
 * "refresh" affordance; nothing calls it in normal operation. */
export function clearJenkinsCache(): void {
  testReportCache.clear();
}

// ---- Requests ----

const RETRYABLE_ATTEMPTS = 3;

/** Jenkins here intermittently drops connections mid-response, and a
 * transient 5xx is just as likely as a real one. Neither is worth failing
 * a whole scan over, so a request gets a couple of quick retries — but an
 * auth problem or a 404 is a settled answer and is never retried. */
function isRetryable(err: unknown): boolean {
  if (err instanceof AuthSetupError) return false;
  if (err instanceof HttpStatusError) return err.status >= 500;
  // An unparseable URL is a settled answer, not a blip — retrying it just
  // multiplies the same failure by three on every request of a scan.
  // Matched narrowly on the code rather than on `instanceof TypeError`,
  // because Node's fetch also rejects with a TypeError ("fetch failed")
  // for real network trouble, which is precisely what should retry.
  if ((err as { code?: string })?.code === "ERR_INVALID_URL") return false;
  return true; // network-level failure
}

async function jenkinsJson(site: SiteAuth, auth: AuthContext, pathAndQuery: string): Promise<unknown> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= RETRYABLE_ATTEMPTS; attempt += 1) {
    try {
      return await authedJson(site, auth, pathAndQuery);
    } catch (err) {
      lastError = err;
      if (!isRetryable(err) || attempt === RETRYABLE_ATTEMPTS) throw err;
      await new Promise((resolve) => setTimeout(resolve, 250 * attempt));
    }
  }
  throw lastError;
}

/** `GET /job/<job>/api/json?tree=builds[<fields>]{0,limit}` — one request
 * for `limit` builds' worth of whatever fields the caller names. */
export async function jobBuilds(
  site: SiteAuth,
  auth: AuthContext,
  job: string,
  fields: string,
  limit: number,
): Promise<JenkinsBuild[]> {
  const tree = encodeURIComponent(`builds[${fields}]{0,${limit}}`);
  const data = (await jenkinsJson(site, auth, `/job/${encodeURIComponent(job)}/api/json?tree=${tree}`)) as {
    builds?: JenkinsBuild[];
  };
  return data?.builds || [];
}

/**
 * A build's junit report, or null when it published none.
 *
 * The 404 is normal, not exceptional: real browser builds #6980 and #6982
 * have no test report at all, and a scan must record that as "unknown"
 * rather than abandoning the whole table.
 *
 * The tree projection here is what makes this affordable. Asked for
 * whole, one of these reports is 0.8 MB (pipeline-triggered, ~1780 cases)
 * to 1.8 MB (nightly, ~3240 cases), almost all of it errorStackTrace,
 * stdout and stderr. None of that is displayed, so none of it is
 * requested; errorDetails — a single line — is kept for the panel.
 */
export async function fetchTestReport(
  site: SiteAuth,
  auth: AuthContext,
  job: string,
  buildNumber: number,
  options: { cacheable: boolean } = { cacheable: true },
): Promise<JenkinsTestReport | null> {
  const key = cacheKey(job, buildNumber);
  const cached = options.cacheable ? testReportCache.get(key) : undefined;
  if (cached) return cached;

  const tree = encodeURIComponent(
    "failCount,passCount,skipCount,suites[name,cases[className,name,status,age,failedSince,errorDetails,skipped]]",
  );
  let report: JenkinsTestReport | null;
  try {
    report = (await jenkinsJson(
      site,
      auth,
      `/job/${encodeURIComponent(job)}/${buildNumber}/testReport/api/json?tree=${tree}`,
    )) as JenkinsTestReport;
  } catch (err) {
    if (err instanceof HttpStatusError && err.status === 404) {
      // Deliberately NOT cached. A 404 here normally means "this build
      // published no test report", but it's indistinguishable from a
      // build Jenkins has since rotated away or a 404 thrown during a
      // Jenkins reload — and remembering one of those for the life of
      // the process would quietly shorten every later walk-back. The
      // response is tiny, so re-asking costs nothing.
      return null;
    }
    throw err;
  }
  if (options.cacheable) testReportCache.set(key, report);
  return report;
}
