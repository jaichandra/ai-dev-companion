// Shared "how does a feature talk to Bitbucket Server/DC" helper, mirroring
// core/jenkins.ts. Auth reuses core/atlassian.ts's authedJson/
// authedJsonWithHeaders wholesale — relayed browser session first, a
// configured API token as fallback, AuthSetupError otherwise — so there's
// exactly one copy of that fallback chain. Bitbucket takes a Bearer token
// (SiteAuth's default authScheme, same convention as Jira — no username
// needed).
//
// Everything here goes through /rest/api/1.0/ (see this task's
// global-constraints.md "Bitbucket REST facts") and returns the normalized
// shapes from Task 4's core/bitbucket-normalize.js. This file's own job is
// only the HTTP plumbing: building paths, following activities' pagination
// (capped — see core/bitbucket-contract.js's followPages), and reading the
// X-AUSERNAME response header.
//
// Doctor's `--bitbucket-contract` check (doctor.js) needs the *raw*
// responses too, to diff their keys against EXPECTED_KEYS — so the raw
// fetchers (fetchPullRequestRaw, fetchActivityPage/fetchAllActivityPages,
// fetchApplicationPropertiesRaw) are exported alongside the normalized
// ones, rather than normalizing inline with no way back to the raw shape.
import { AuthContext, HttpStatusError, SiteAuth, authedJson, authedJsonWithHeaders } from "./atlassian";
import { Config, DEFAULT_BITBUCKET_BASE_URL } from "../config";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./credentials.js") as { getToken(name: string): string | undefined };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const externalTokens = (require("./external-tokens.js") as { createExternalTokens(): { tokenFor(name: string, baseUrl: string): { token: string } | undefined } }).createExternalTokens();

// eslint-disable-next-line @typescript-eslint/no-var-requires
const normalize = require("./bitbucket-normalize.js") as {
  normalizePullRequest(raw: unknown): Record<string, unknown> | null;
  normalizeActivities(pages: unknown): Record<string, unknown>[];
  normalizeAppProperties(raw: unknown): { version: string | null; major: number; minor: number } | null;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const contract = require("./bitbucket-contract.js") as {
  followPages(
    fetchPage: (start: number) => Promise<RawActivityPage>,
    opts?: { maxPages?: number },
  ): Promise<{ pages: RawActivityPage[]; truncated: boolean; malformed?: boolean }>;
};

export interface RawActivityPage {
  values?: unknown[];
  isLastPage?: boolean;
  nextPageStart?: number;
  size?: number;
  start?: number;
  limit?: number;
}

/** Trailing slashes off, and a scheme added if the configured value is
 * missing one — same reasoning as core/jenkins.ts's normalizeBaseUrl: the
 * setup wizard / Settings panel take the base URL as free text. */
function normalizeBaseUrl(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, "");
  return /^https?:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
}

/** Bitbucket as an authenticated site. Bearer token (SiteAuth's default
 * authScheme) — Bitbucket Server's REST convention, same as Jira's. */
export function bitbucketSite(config: Config): SiteAuth {
  const baseUrl = normalizeBaseUrl(config.bitbucket?.baseUrl || DEFAULT_BITBUCKET_BASE_URL);
  return {
    baseUrl,
    label: "Bitbucket",
    configKey: "bitbucket.apiToken",
    // Own saved token first, then the one another tool already stored for this host (core/external-tokens.js).
    apiToken: credentials.getToken("bitbucket.apiToken") ?? externalTokens.tokenFor("bitbucket.apiToken", baseUrl)?.token,
  };
}

const API_ROOT = "/rest/api/1.0";
// Mirrors core/jenkins.ts-style caps: a PR with an unusually long activity
// history shouldn't turn "collect its review comments" into an unbounded
// fetch — see listActivities' WARN-not-fail handling below.
const MAX_ACTIVITY_PAGES = 20;

function prPath(project: string, repo: string, prId: number, suffix = ""): string {
  return `${API_ROOT}/projects/${encodeURIComponent(project)}/repos/${encodeURIComponent(repo)}/pull-requests/${prId}${suffix}`;
}

/** `GET .../pull-requests/{id}`, unnormalized — exported for doctor's live
 * contract check, which diffs the raw keys against EXPECTED_KEYS rather
 * than the normalized shape. */
export function fetchPullRequestRaw(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
): Promise<unknown> {
  return authedJson(site, auth, prPath(project, repo, prId));
}

/** The normalized PR — see core/bitbucket-normalize.js's
 * normalizePullRequest. */
export async function getPullRequest(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
): Promise<Record<string, unknown> | null> {
  return normalize.normalizePullRequest(await fetchPullRequestRaw(site, auth, project, repo, prId));
}

/** One page of `GET .../pull-requests/{id}/activities`, unnormalized. */
export function fetchActivityPage(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
  start: number,
): Promise<RawActivityPage> {
  const query = start ? `?start=${start}` : "";
  return authedJson(site, auth, prPath(project, repo, prId, `/activities${query}`)) as Promise<RawActivityPage>;
}

/**
 * Every activities page for this PR, raw — follows `nextPageStart` until
 * `isLastPage`, capped at MAX_ACTIVITY_PAGES (see
 * core/bitbucket-contract.js's followPages, shared with its own tests).
 * Exported for doctor's contract check; listActivities below is the
 * normalized counterpart every feature actually uses.
 */
export function fetchAllActivityPages(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
): Promise<{ pages: RawActivityPage[]; truncated: boolean; malformed?: boolean }> {
  return contract.followPages((start) => fetchActivityPage(site, auth, project, repo, prId, start), {
    maxPages: MAX_ACTIVITY_PAGES,
  });
}

/**
 * Every review comment on this PR, normalized. Logs a WARN — never fails
 * the request — if MAX_ACTIVITY_PAGES is hit before the last page, per
 * this task's ruling: an unusually long activity history should be
 * reported as capped, not turned into a broken feature.
 */
export async function listActivities(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
): Promise<Record<string, unknown>[]> {
  const { pages, truncated } = await fetchAllActivityPages(site, auth, project, repo, prId);
  if (truncated) {
    console.warn(
      `[bitbucket] Hit the ${MAX_ACTIVITY_PAGES}-page cap fetching activities for ` +
        `${project}/${repo}#${prId} — some older comments may be missing.`,
    );
  }
  return normalize.normalizeActivities(pages);
}

/** `GET /application-properties`, unnormalized — exported for doctor's
 * contract check and version tracking. */
export function fetchApplicationPropertiesRaw(site: SiteAuth, auth: AuthContext): Promise<unknown> {
  return authedJson(site, auth, `${API_ROOT}/application-properties`);
}

/** The normalized `{version, major, minor}` — see
 * core/bitbucket-normalize.js's normalizeAppProperties. */
export async function applicationProperties(
  site: SiteAuth,
  auth: AuthContext,
): Promise<{ version: string | null; major: number; minor: number } | null> {
  return normalize.normalizeAppProperties(await fetchApplicationPropertiesRaw(site, auth));
}

/**
 * The logged-in Bitbucket username, from the X-AUSERNAME response header
 * of a cheap authenticated call (application-properties needs no special
 * permission and returns almost nothing). Returns null — not an error —
 * when the header is absent, rather than treating that as a failure.
 */
export async function whoAmI(site: SiteAuth, auth: AuthContext): Promise<string | null> {
  const { headers } = await authedJsonWithHeaders(site, auth, `${API_ROOT}/application-properties`);
  return headers.get("x-ausername") || null;
}

/**
 * Posts a reply under `parentId` and returns the created comment's id, or
 * null if the response doesn't carry a numeric one — read defensively
 * since a reply's own shape isn't one of Task 4's normalizers (only its id
 * is ever needed, to let a caller thread a further reply under it).
 */
export async function replyToComment(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
  parentId: number,
  text: string,
): Promise<number | null> {
  const body = await authedJson(site, auth, prPath(project, repo, prId, "/comments"), {
    method: "POST",
    body: { text, parent: { id: parentId } },
  });
  const id = body && typeof body === "object" ? (body as { id?: unknown }).id : undefined;
  return typeof id === "number" ? id : null;
}

// ---- Phase 6: what the ticket workspace and ticket-to-PR read and write ----

/** A PR as the ticket features see it — core/bitbucket-normalize.js's
 * normalizePullRequestSummary. */
export interface PullRequestSummary {
  id: number;
  title: string | null;
  state: string | null;
  fromBranch: string | null;
  fromSha: string | null;
  toBranch: string | null;
  url: string | null;
  updatedAt: number | null;
}

export interface BuildSummary {
  state: "FAILED" | "INPROGRESS" | "SUCCESSFUL" | null;
  counts: Record<string, number>;
  builds: { state: string; key: string | null; name: string | null; url: string | null; at: number | null }[];
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const ticketNormalize = require("./bitbucket-normalize.js") as {
  normalizePullRequestSummary(raw: unknown): PullRequestSummary | null;
  normalizePullRequestList(raw: unknown): PullRequestSummary[];
  normalizeBuildStatuses(raw: unknown): BuildSummary;
  normalizeDefaultBranch(raw: unknown): string | null;
  normalizeRepository(raw: unknown): { id: number; slug: string | null; projectKey: string | null } | null;
  normalizeDefaultReviewers(raw: unknown): string[];
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const endpoints = require("./bitbucket-endpoints.js") as {
  defaultBranchPaths(project: string, repo: string): string[];
  branchPullRequestsPath(project: string, repo: string, branch: string, opts?: { state?: string; limit?: number }): string;
  commitBuildsPaths(project: string, repo: string, sha: string): string[];
  repoPath(project: string, repo: string): string;
  defaultReviewersPath(
    project: string,
    repo: string,
    ids: { sourceRepoId: number; targetRepoId: number; sourceBranch: string; targetBranch: string },
  ): string;
  createPullRequestPath(project: string, repo: string): string;
  createPullRequestBody(args: {
    project: string;
    repo: string;
    title: string;
    description: string;
    fromBranch: string;
    toBranch: string;
    reviewers?: string[];
  }): unknown;
  firstAvailable(
    paths: string[],
    fetchPath: (path: string) => Promise<unknown>,
    isNotFound: (err: unknown) => boolean,
  ): Promise<{ path: string; body: unknown }>;
};

const isNotFound = (err: unknown): boolean => err instanceof HttpStatusError && err.status === 404;

/** The repo's default branch per Bitbucket (clones often have no
 * origin/HEAD), or null when neither endpoint exists on this version. */
export async function getDefaultBranch(site: SiteAuth, auth: AuthContext, project: string, repo: string): Promise<string | null> {
  try {
    const { body } = await endpoints.firstAvailable(
      endpoints.defaultBranchPaths(project, repo),
      (p) => authedJson(site, auth, p),
      isNotFound,
    );
    return ticketNormalize.normalizeDefaultBranch(body);
  } catch (err) {
    if (isNotFound(err)) return null;
    throw err;
  }
}

/** PRs (any state, newest first) whose source branch is `branch`. */
export async function listBranchPullRequests(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  branch: string,
): Promise<PullRequestSummary[]> {
  return ticketNormalize.normalizePullRequestList(
    await authedJson(site, auth, endpoints.branchPullRequestsPath(project, repo, branch)),
  );
}

/** Build statuses of a commit, from the build-status plugin. Null when the
 * endpoint doesn't exist. */
export async function getCommitBuildStatus(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  sha: string,
): Promise<BuildSummary | null> {
  try {
    const { body } = await endpoints.firstAvailable(
      endpoints.commitBuildsPaths(project, repo, sha),
      (p) => authedJson(site, auth, p),
      isNotFound,
    );
    return ticketNormalize.normalizeBuildStatuses(body);
  } catch (err) {
    if (isNotFound(err)) return null;
    throw err;
  }
}

export async function getRepositoryId(site: SiteAuth, auth: AuthContext, project: string, repo: string): Promise<number> {
  const r = ticketNormalize.normalizeRepository(await authedJson(site, auth, endpoints.repoPath(project, repo)));
  if (!r) throw new Error(`Bitbucket didn't return an id for ${project}/${repo}.`);
  return r.id;
}

/** Usernames the repo's default-reviewer conditions pick for this branch pair. */
export async function getDefaultReviewers(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  ids: { sourceRepoId: number; targetRepoId: number; sourceBranch: string; targetBranch: string },
): Promise<string[]> {
  return ticketNormalize.normalizeDefaultReviewers(
    await authedJson(site, auth, endpoints.defaultReviewersPath(project, repo, ids)),
  );
}

// ---- Phase 7: the watchers and the digest ----

/** A dashboard PR (core/bitbucket-normalize.js's normalizeDashboardPullRequest). */
export interface DashboardPullRequest extends PullRequestSummary {
  project: string;
  repo: string;
  toSha: string | null;
  /** Where the source branch lives (a fork when it differs from project/repo). */
  fromRepo?: { projectKey: string | null; slug: string | null };
  authorSlug: string | null;
  reviewers: { name: string; status: "APPROVED" | "NEEDS_WORK" | "UNAPPROVED" | "unknown" }[];
  approvals: number;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const dashboard = require("./bitbucket-normalize.js") as {
  normalizeDashboardPullRequests(raw: unknown): DashboardPullRequest[];
  normalizeMergeStatus(raw: unknown): { conflicted: boolean | null; canMerge: boolean | null; outcome: string | null };
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const dashboardEndpoints = require("./bitbucket-endpoints.js") as {
  dashboardPullRequestsPath(opts: { role: "AUTHOR" | "REVIEWER"; participantStatus?: "UNAPPROVED"; limit?: number }): string;
  mergeStatusPath(project: string, repo: string, prId: number): string;
};

/** Your open PRs (role AUTHOR), or the ones waiting on your review (REVIEWER, unapproved). */
export async function listDashboardPullRequests(
  site: SiteAuth,
  auth: AuthContext,
  role: "AUTHOR" | "REVIEWER",
): Promise<DashboardPullRequest[]> {
  const path = dashboardEndpoints.dashboardPullRequestsPath(
    role === "REVIEWER" ? { role, participantStatus: "UNAPPROVED" } : { role },
  );
  return dashboard.normalizeDashboardPullRequests(await authedJson(site, auth, path));
}

export async function getMergeStatus(
  site: SiteAuth,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
): Promise<{ conflicted: boolean | null; canMerge: boolean | null; outcome: string | null }> {
  return dashboard.normalizeMergeStatus(await authedJson(site, auth, dashboardEndpoints.mergeStatusPath(project, repo, prId)));
}

/** Opens a same-repo PR. Returns it normalized (with its page URL). */
export async function createPullRequest(
  site: SiteAuth,
  auth: AuthContext,
  args: { project: string; repo: string; title: string; description: string; fromBranch: string; toBranch: string; reviewers: string[] },
): Promise<PullRequestSummary> {
  const created = await authedJson(site, auth, endpoints.createPullRequestPath(args.project, args.repo), {
    method: "POST",
    body: endpoints.createPullRequestBody(args),
  });
  const pr = ticketNormalize.normalizePullRequestSummary(created);
  if (!pr || typeof pr.id !== "number") throw new Error("Bitbucket didn't return the new pull request.");
  return pr;
}
