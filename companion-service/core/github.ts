// GitHub (github.com or GitHub Enterprise) as a git host, over its REST and GraphQL APIs: the counterpart
// of core/bitbucket.ts, returning the same shapes (core/github-normalize.js does the mapping) so the
// features never learn which host they are on. GitHub's API is reached with a personal access token
// only — the browser's github.com session doesn't authenticate it — so every call goes through a
// token-only SiteAuth (core/atlassian.ts), and the web address (links, the extension's page) is kept
// apart from the API address.
import { AuthContext, HttpStatusError, SiteAuth, authedJson } from "./atlassian";
import type { BuildSummary, DashboardPullRequest, PullRequestSummary } from "./bitbucket";
import type { Config } from "../config";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./credentials.js") as { getToken(name: string): string | undefined };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const externalTokens = (require("./external-tokens.js") as { createExternalTokens(): { tokenFor(name: string, baseUrl: string): { token: string } | undefined } }).createExternalTokens();
// eslint-disable-next-line @typescript-eslint/no-var-requires
const environment = require("../environment.js") as { defaultBaseUrl(id: string): string };

// eslint-disable-next-line @typescript-eslint/no-var-requires
const endpoints = require("./github-endpoints.js") as {
  API_HEADERS: Record<string, string>;
  apiBases(webBaseUrl: string): { rest: string; graphql: string };
  userPath(): string;
  repoPath(owner: string, repo: string): string;
  pullPath(owner: string, repo: string, n: number): string;
  createPullPath(owner: string, repo: string): string;
  requestedReviewersPath(owner: string, repo: string, n: number): string;
  replyPath(owner: string, repo: string, n: number, commentId: number): string;
  branchPullsPath(owner: string, repo: string, branch: string, opts?: { limit?: number }): string;
  checkRunsPath(owner: string, repo: string, sha: string): string;
  combinedStatusPath(owner: string, repo: string, sha: string): string;
  createPullBody(args: { owner: string; repo: string; title: string; description: string; fromBranch: string; toBranch: string }): unknown;
  REVIEW_THREADS_QUERY: string;
  DASHBOARD_QUERY: string;
  dashboardSearch(role: "AUTHOR" | "REVIEWER", login: string): string;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const normalize = require("./github-normalize.js") as {
  normalizePullRequest(raw: unknown): Record<string, unknown> | null;
  normalizePullRequestSummary(raw: unknown): PullRequestSummary | null;
  normalizePullRequestList(raw: unknown): PullRequestSummary[];
  normalizeMergeStatus(raw: unknown): { conflicted: boolean | null; canMerge: boolean | null; outcome: string | null };
  normalizeRepository(raw: unknown): { id: number; slug: string | null; projectKey: string | null } | null;
  normalizeDefaultBranch(raw: unknown): string | null;
  normalizeReviewThreads(pages: unknown): Record<string, unknown>[];
  normalizeBuildStatus(parts: { checkRuns: unknown; status: unknown }): BuildSummary | null;
  normalizeDashboard(raw: unknown): DashboardPullRequest[];
};

/** A GitHub site: its web address (for links and the extension) and the token-only sites its APIs are reached through. */
export interface GithubSite {
  web: string;
  rest: SiteAuth;
  graphql: SiteAuth;
}

/** Trailing slashes off, and a scheme added if the configured value is missing one. */
function normalizeBaseUrl(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, "");
  return /^https?:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
}

/** The GitHub site configured under `siteId` in config.json (the profile's git site). */
export function githubSite(config: Config, siteId = "github"): GithubSite {
  const configured = (config as unknown as Record<string, { baseUrl?: string } | undefined>)[siteId]?.baseUrl;
  const web = normalizeBaseUrl(configured || environment.defaultBaseUrl(siteId) || "https://github.com");
  const configKey = `${siteId}.apiToken`;
  const apiToken = credentials.getToken(configKey) ?? externalTokens.tokenFor(configKey, web)?.token;
  const bases = endpoints.apiBases(web);
  const common = { label: "GitHub", configKey, apiToken, tokenOnly: true, headers: endpoints.API_HEADERS };
  return { web, rest: { ...common, baseUrl: bases.rest }, graphql: { ...common, baseUrl: bases.graphql } };
}

const isNotFound = (err: unknown): boolean => err instanceof HttpStatusError && err.status === 404;

/** One GraphQL call. GitHub answers 200 with `errors` for a bad query or a missing repository. */
async function graphql(site: GithubSite, auth: AuthContext, query: string, variables: Record<string, unknown>): Promise<unknown> {
  const result = (await authedJson(site.graphql, auth, "", { method: "POST", body: { query, variables } })) as {
    data?: unknown;
    errors?: { message?: string }[];
  };
  if (Array.isArray(result?.errors) && result.errors.length > 0 && !result.data) {
    throw new HttpStatusError(`GitHub GraphQL request failed: ${result.errors[0].message || "unknown error"}`, 200);
  }
  return result;
}

/** The signed-in user's login, lower-cased — what comments' and reviewers' names are compared with. Null when unknown. */
export async function whoAmI(site: GithubSite, auth: AuthContext): Promise<string | null> {
  const user = (await authedJson(site.rest, auth, endpoints.userPath())) as { login?: unknown } | null;
  return typeof user?.login === "string" && user.login ? user.login.toLowerCase() : null;
}

export async function getPullRequest(site: GithubSite, auth: AuthContext, owner: string, repo: string, n: number): Promise<Record<string, unknown> | null> {
  return normalize.normalizePullRequest(await authedJson(site.rest, auth, endpoints.pullPath(owner, repo, n)));
}

/** Review threads are paged 100 at a time; five pages is the most one PR is asked for. */
const MAX_THREAD_PAGES = 5;

export async function listActivities(site: GithubSite, auth: AuthContext, owner: string, repo: string, n: number): Promise<Record<string, unknown>[]> {
  endpoints.pullPath(owner, repo, n); // validates the arguments before any request
  const pages: unknown[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < MAX_THREAD_PAGES; page += 1) {
    const result = (await graphql(site, auth, endpoints.REVIEW_THREADS_QUERY, { owner, name: repo, number: n, cursor })) as {
      data?: { repository?: { pullRequest?: { reviewThreads?: { pageInfo?: { hasNextPage?: boolean; endCursor?: string } } } } };
    };
    pages.push(result);
    const info = result.data?.repository?.pullRequest?.reviewThreads?.pageInfo;
    if (!info?.hasNextPage || !info.endCursor) break;
    cursor = info.endCursor;
    if (page === MAX_THREAD_PAGES - 1) console.warn(`[github] ${owner}/${repo}#${n} has more than ${MAX_THREAD_PAGES * 100} review threads; the rest are not read.`);
  }
  return normalize.normalizeReviewThreads(pages);
}

/** Replies in the thread of review comment `parentId`. Returns the new comment's id, or null. */
export async function replyToComment(site: GithubSite, auth: AuthContext, owner: string, repo: string, n: number, parentId: number, text: string): Promise<number | null> {
  const created = (await authedJson(site.rest, auth, endpoints.replyPath(owner, repo, n, parentId), { method: "POST", body: { body: text } })) as { id?: unknown } | null;
  return typeof created?.id === "number" ? created.id : null;
}

export async function getDefaultBranch(site: GithubSite, auth: AuthContext, owner: string, repo: string): Promise<string | null> {
  try {
    return normalize.normalizeDefaultBranch(await authedJson(site.rest, auth, endpoints.repoPath(owner, repo)));
  } catch (err) {
    if (isNotFound(err)) return null;
    throw err;
  }
}

export async function listBranchPullRequests(site: GithubSite, auth: AuthContext, owner: string, repo: string, branch: string): Promise<PullRequestSummary[]> {
  return normalize.normalizePullRequestList(await authedJson(site.rest, auth, endpoints.branchPullsPath(owner, repo, branch)));
}

export async function createPullRequest(
  site: GithubSite,
  auth: AuthContext,
  args: { project: string; repo: string; title: string; description: string; fromBranch: string; toBranch: string; reviewers: string[] },
): Promise<PullRequestSummary> {
  const body = endpoints.createPullBody({ owner: args.project, repo: args.repo, title: args.title, description: args.description, fromBranch: args.fromBranch, toBranch: args.toBranch });
  const created = (await authedJson(site.rest, auth, endpoints.createPullPath(args.project, args.repo), { method: "POST", body })) as { number?: unknown } | null;
  const summary = normalize.normalizePullRequestSummary(created);
  if (!summary) throw new Error("GitHub didn't return the new pull request.");
  // Reviewers are a separate call on GitHub. The PR exists either way, so a refused reviewer (someone
  // without access, or the author themselves) doesn't fail the create.
  if (args.reviewers.length > 0) {
    try {
      await authedJson(site.rest, auth, endpoints.requestedReviewersPath(args.project, args.repo, summary.id), { method: "POST", body: { reviewers: args.reviewers } });
    } catch (err) {
      console.warn(`[github] couldn't request reviewers on ${args.project}/${args.repo}#${summary.id}: ${(err as Error).message}`);
    }
  }
  return summary;
}

export async function getRepositoryId(site: GithubSite, auth: AuthContext, owner: string, repo: string): Promise<number> {
  const found = normalize.normalizeRepository(await authedJson(site.rest, auth, endpoints.repoPath(owner, repo)));
  if (!found) throw new Error("GitHub didn't return the repository.");
  return found.id;
}

/** GitHub has no default reviewers (CODEOWNERS requests them itself when the PR opens). */
export async function getDefaultReviewers(): Promise<string[]> {
  return [];
}

async function optional(site: GithubSite, auth: AuthContext, path: string): Promise<unknown> {
  try {
    return await authedJson(site.rest, auth, path);
  } catch (err) {
    if (isNotFound(err)) return null;
    throw err;
  }
}

/** The commit's check runs and commit statuses together; null when it has none. */
export async function getCommitBuildStatus(site: GithubSite, auth: AuthContext, owner: string, repo: string, sha: string): Promise<BuildSummary | null> {
  const checkRuns = await optional(site, auth, endpoints.checkRunsPath(owner, repo, sha));
  const status = await optional(site, auth, endpoints.combinedStatusPath(owner, repo, sha));
  return normalize.normalizeBuildStatus({ checkRuns, status });
}

/** Your open PRs (AUTHOR), or the ones waiting on your review (REVIEWER). */
export async function listDashboardPullRequests(site: GithubSite, auth: AuthContext, role: "AUTHOR" | "REVIEWER"): Promise<DashboardPullRequest[]> {
  const me = await whoAmI(site, auth);
  if (!me) return [];
  return normalize.normalizeDashboard(await graphql(site, auth, endpoints.DASHBOARD_QUERY, { search: endpoints.dashboardSearch(role, me) }));
}

/**
 * Whether the PR conflicts with its target. GitHub works this out lazily: the first read of a PR can answer
 * "not known yet" (null) and start the computation, so an unknown answer is asked again a couple of times
 * before giving up — and "unknown" stays unknown rather than being read as "no conflict".
 */
export async function getMergeStatus(
  site: GithubSite,
  auth: AuthContext,
  owner: string,
  repo: string,
  n: number,
  sleep: (ms: number) => Promise<void> = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
): Promise<{ conflicted: boolean | null; canMerge: boolean | null; outcome: string | null }> {
  let status = normalize.normalizeMergeStatus(await authedJson(site.rest, auth, endpoints.pullPath(owner, repo, n)));
  for (let attempt = 0; attempt < 2 && status.conflicted === null; attempt += 1) {
    await sleep(700 * (attempt + 1));
    status = normalize.normalizeMergeStatus(await authedJson(site.rest, auth, endpoints.pullPath(owner, repo, n)));
  }
  return status;
}
