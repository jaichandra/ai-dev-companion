// The Bitbucket Server/DC paths the ticket features call, and the fallback
// rule for endpoints that moved between versions. Pure: core/bitbucket.ts
// does the HTTP. Pinned to /rest/api/1.0 (never /latest); plugin endpoints
// (build-status, default-reviewers) keep their own versioned roots. Every
// project/repo segment and branch is checked here, so nothing untrusted is
// ever spliced into a path.
const { isSafeRepoSegment } = require("./prereqs.js");

const API_ROOT = "/rest/api/1.0";
const SHA_RE = /^[0-9a-f]{7,40}$/i;
// A plain branch name: no leading "-", no "..", no "//", no trailing "/"
// or "." or ".lock", no spaces, quotes or shell/ref metacharacters.
const BRANCH_RE = /^[A-Za-z0-9][A-Za-z0-9._/-]{0,199}$/;

function isSafeBranchName(name) {
  return (
    typeof name === "string" &&
    BRANCH_RE.test(name) &&
    !name.includes("..") &&
    !name.includes("//") &&
    !name.endsWith("/") &&
    !name.endsWith(".") &&
    !name.endsWith(".lock") &&
    !name.includes("@{")
  );
}

function assertRepo(project, repo) {
  if (!isSafeRepoSegment(project) || !isSafeRepoSegment(repo)) {
    throw new Error(`Not a safe Bitbucket project/repo: ${String(project)}/${String(repo)}`);
  }
}

function assertBranch(branch) {
  if (!isSafeBranchName(branch)) throw new Error(`Not a safe branch name: ${String(branch).slice(0, 80)}`);
}

function repoPath(project, repo) {
  assertRepo(project, repo);
  return `${API_ROOT}/projects/${encodeURIComponent(project)}/repos/${encodeURIComponent(repo)}`;
}

/** Newest endpoint first; `/branches/default` is the pre-7.5 spelling. */
function defaultBranchPaths(project, repo) {
  const base = repoPath(project, repo);
  return [`${base}/default-branch`, `${base}/branches/default`];
}

/** PRs whose source is `branch` in this repo (any state, newest first). */
function branchPullRequestsPath(project, repo, branch, { state = "ALL", limit = 10 } = {}) {
  assertBranch(branch);
  const query = new URLSearchParams({
    state,
    direction: "OUTGOING",
    at: `refs/heads/${branch}`,
    order: "NEWEST",
    limit: String(limit),
  });
  return `${repoPath(project, repo)}/pull-requests?${query}`;
}

/** The build-status plugin. The `/commits/{sha}/builds` spelling isn't used: on
 * Bitbucket DC 9.4 it answers 400 ("non-blank key"), not 404, so a fallback
 * never happens. Kept as a list so callers still go through firstAvailable. */
function commitBuildsPaths(project, repo, sha) {
  if (typeof sha !== "string" || !SHA_RE.test(sha)) throw new Error("Not a commit id.");
  return [`/rest/build-status/1.0/commits/${sha}`];
}

/** REST PR creation doesn't add default reviewers, so they're asked for here. */
function defaultReviewersPath(project, repo, { sourceRepoId, targetRepoId, sourceBranch, targetBranch }) {
  assertRepo(project, repo);
  assertBranch(sourceBranch);
  assertBranch(targetBranch);
  if (!Number.isSafeInteger(sourceRepoId) || !Number.isSafeInteger(targetRepoId)) throw new Error("Not a repository id.");
  const query = new URLSearchParams({
    sourceRepoId: String(sourceRepoId),
    targetRepoId: String(targetRepoId),
    sourceRefId: `refs/heads/${sourceBranch}`,
    targetRefId: `refs/heads/${targetBranch}`,
  });
  return `/rest/default-reviewers/1.0/projects/${encodeURIComponent(project)}/repos/${encodeURIComponent(repo)}/reviewers?${query}`;
}

function createPullRequestPath(project, repo) {
  return `${repoPath(project, repo)}/pull-requests`;
}

/** The POST body for a same-repo PR from `fromBranch` into `toBranch`. */
function createPullRequestBody({ project, repo, title, description, fromBranch, toBranch, reviewers = [] }) {
  assertRepo(project, repo);
  assertBranch(fromBranch);
  assertBranch(toBranch);
  const ref = (branch) => ({ id: `refs/heads/${branch}`, repository: { slug: repo, project: { key: project } } });
  return {
    title,
    description,
    fromRef: ref(fromBranch),
    toRef: ref(toBranch),
    reviewers: reviewers.map((name) => ({ user: { name } })),
  };
}

/**
 * Tries each path in turn with `fetchPath(path)` and returns
 * `{ path, body }` from the first that answers. Only a "not found" error
 * (per `isNotFound`) moves on to the next path; anything else — auth, 5xx —
 * is thrown at once, and so is the last path's error.
 */
async function firstAvailable(paths, fetchPath, isNotFound) {
  let lastError;
  for (const path of paths) {
    try {
      return { path, body: await fetchPath(path) };
    } catch (err) {
      if (!isNotFound(err)) throw err;
      lastError = err;
    }
  }
  throw lastError || new Error("No endpoint to try.");
}

// ---- Phase 7: the watchers and the digest ----

const DASHBOARD_ROLES = ["AUTHOR", "REVIEWER", "PARTICIPANT"];
const PARTICIPANT_STATUSES = ["UNAPPROVED", "NEEDS_WORK", "APPROVED"];

/** Your open PRs across every repo: as author, or waiting on your review
 * (`participantStatus: "UNAPPROVED"`). */
function dashboardPullRequestsPath({ role, participantStatus, limit = 50 } = {}) {
  if (!DASHBOARD_ROLES.includes(role)) throw new Error(`Not a dashboard role: ${String(role).slice(0, 20)}`);
  const query = new URLSearchParams({ role, state: "OPEN", order: "NEWEST", limit: String(Math.max(1, Math.min(limit, 100))) });
  if (participantStatus !== undefined) {
    if (!PARTICIPANT_STATUSES.includes(participantStatus)) throw new Error("Not a participant status.");
    query.set("participantStatus", participantStatus);
  }
  return `${API_ROOT}/dashboard/pull-requests?${query}`;
}

/** Whether a PR can merge — `{canMerge, conflicted, outcome, vetoes}`. */
function mergeStatusPath(project, repo, prId) {
  if (!Number.isSafeInteger(prId) || prId < 1) throw new Error("Not a pull request id.");
  return `${repoPath(project, repo)}/pull-requests/${prId}/merge`;
}

module.exports = {
  API_ROOT,
  isSafeBranchName,
  repoPath,
  defaultBranchPaths,
  branchPullRequestsPath,
  commitBuildsPaths,
  defaultReviewersPath,
  createPullRequestPath,
  createPullRequestBody,
  firstAvailable,
  dashboardPullRequestsPath,
  mergeStatusPath,
};
