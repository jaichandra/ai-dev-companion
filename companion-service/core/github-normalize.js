// GitHub's JSON turned into the shapes the features already read (the same ones core/bitbucket-normalize.js
// produces for Bitbucket), so nothing downstream knows which host it is talking to. Pure and defensive: a
// missing or mistyped field becomes null (or an "unknown" enum), never a throw.
//
// Mapping notes:
//   project = the repository owner, repo = the repository name, prId = the pull request number.
//   state   = "OPEN" | "MERGED" | "DECLINED" (GitHub's closed-without-merge).
//   logins  = lower-cased wherever they are compared (authorSlug, whoAmI), as GitHub logins are case-insensitive.

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v) => (typeof v === "string" && v ? v : null);
const num = (v) => (typeof v === "number" && Number.isFinite(v) ? v : null);
const login = (user) => (isObject(user) && str(user.login) ? user.login.toLowerCase() : null);
const ms = (iso) => {
  const t = typeof iso === "string" ? Date.parse(iso) : NaN;
  return Number.isNaN(t) ? null : t;
};

/** { projectKey, slug } of a repository object ({ name, owner: { login } }); both null when it is gone (a deleted fork). */
function repoRef(repo) {
  if (!isObject(repo)) return { projectKey: null, slug: null };
  const owner = isObject(repo.owner) ? str(repo.owner.login) : null;
  return { projectKey: owner, slug: str(repo.name) };
}

function stateOf(pr) {
  if (pr.merged === true || str(pr.merged_at)) return "MERGED";
  if (pr.state === "closed") return "DECLINED";
  if (pr.state === "open") return "OPEN";
  return null;
}

/** The PR as getPullRequest returns it (REST `GET /pulls/{n}`). */
function normalizePullRequest(raw) {
  if (!isObject(raw)) return null;
  const head = isObject(raw.head) ? raw.head : {};
  const base = isObject(raw.base) ? raw.base : {};
  const out = {
    id: num(raw.number),
    version: null,
    title: str(raw.title),
    state: stateOf(raw),
    fromBranch: str(head.ref),
    fromSha: str(head.sha),
    fromRepo: repoRef(head.repo),
    toBranch: str(base.ref),
    toSha: str(base.sha),
    toRepo: repoRef(base.repo),
    authorSlug: login(raw.user),
  };
  const commentCount = (num(raw.comments) ?? 0) + (num(raw.review_comments) ?? 0);
  if (num(raw.comments) !== null || num(raw.review_comments) !== null) out.commentCount = commentCount;
  return out;
}

/** A PR as the ticket features list it: the normalized PR plus its address and last update. */
function normalizePullRequestSummary(raw) {
  const pr = normalizePullRequest(raw);
  if (!pr || pr.id === null) return null;
  return {
    id: pr.id,
    title: pr.title,
    state: pr.state,
    fromBranch: pr.fromBranch,
    fromSha: pr.fromSha,
    toBranch: pr.toBranch,
    url: str(raw.html_url),
    updatedAt: ms(raw.updated_at),
  };
}

function normalizePullRequestList(raw) {
  return (Array.isArray(raw) ? raw : []).map(normalizePullRequestSummary).filter(Boolean);
}

/** `GET /pulls/{n}`'s merge fields. `mergeable` is null while GitHub is still working it out. */
function normalizeMergeStatus(raw) {
  if (!isObject(raw)) return { conflicted: null, canMerge: null, outcome: null };
  const state = str(raw.mergeable_state);
  let conflicted = null;
  if (raw.mergeable === false || state === "dirty") conflicted = true;
  else if (raw.mergeable === true) conflicted = false;
  let canMerge = null;
  if (raw.mergeable === true && (state === "clean" || state === "unstable" || state === "has_hooks")) canMerge = true;
  else if (raw.mergeable === false || state === "blocked" || state === "dirty" || state === "behind" || state === "draft") canMerge = false;
  return { conflicted, canMerge, outcome: state ? state.toUpperCase() : null };
}

/** `GET /repos/{o}/{r}`'s id, name and owner. */
function normalizeRepository(raw) {
  if (!isObject(raw) || num(raw.id) === null) return null;
  const ref = repoRef(raw);
  return { id: raw.id, slug: ref.slug, projectKey: ref.projectKey };
}

function normalizeDefaultBranch(raw) {
  return isObject(raw) ? str(raw.default_branch) : null;
}

// ---- Review threads (GraphQL) ----

function normalizeThreadComment(c) {
  return {
    id: num(c.databaseId),
    version: null,
    text: str(c.body),
    authorSlug: login(c.author),
    authorName: isObject(c.author) ? str(c.author.login) : null,
    createdAt: ms(c.createdAt),
    severity: "NORMAL",
    state: "OPEN",
    threadResolved: null,
    anchor: null,
    replies: [],
  };
}

/**
 * One review thread as a ReviewComment: the first comment is the root and the rest are its replies.
 * A resolved thread says so (state "RESOLVED"); an outdated one (its lines changed) is "orphaned", which
 * the address-review-comments feature skips, as it skips Bitbucket's.
 */
function normalizeReviewThread(thread) {
  if (!isObject(thread)) return null;
  const comments = (isObject(thread.comments) && Array.isArray(thread.comments.nodes) ? thread.comments.nodes : []).filter(isObject);
  if (comments.length === 0) return null;
  const root = normalizeThreadComment(comments[0]);
  if (root.id === null) return null;
  const resolved = thread.isResolved === true;
  const left = thread.diffSide === "LEFT";
  root.state = resolved ? "RESOLVED" : "OPEN";
  root.threadResolved = typeof thread.isResolved === "boolean" ? thread.isResolved : null;
  root.anchor = {
    path: str(thread.path),
    line: num(thread.line) ?? num(thread.originalLine),
    lineType: left ? "REMOVED" : "unknown",
    fileType: left ? "FROM" : "TO",
    diffType: "EFFECTIVE",
    orphaned: thread.isOutdated === true,
  };
  root.replies = comments
    .slice(1)
    .map(normalizeThreadComment)
    .filter((c) => c.id !== null);
  return root;
}

/** The ReviewComment list of one or more GraphQL pages (`data.repository.pullRequest.reviewThreads`), deduped by id. */
function normalizeReviewThreads(pages) {
  const seen = new Set();
  const out = [];
  for (const page of Array.isArray(pages) ? pages : [pages]) {
    const nodes = page?.data?.repository?.pullRequest?.reviewThreads?.nodes;
    for (const node of Array.isArray(nodes) ? nodes : []) {
      const comment = normalizeReviewThread(node);
      if (comment && !seen.has(comment.id)) {
        seen.add(comment.id);
        out.push(comment);
      }
    }
  }
  return out;
}

// ---- Build state ----

const CHECK_OK = new Set(["success", "neutral", "skipped"]);
const CHECK_BAD = new Set(["failure", "timed_out", "cancelled", "action_required", "stale", "startup_failure"]);

function checkRunState(run) {
  if (run.status !== "completed") return "INPROGRESS";
  if (CHECK_OK.has(run.conclusion)) return "SUCCESSFUL";
  if (CHECK_BAD.has(run.conclusion)) return "FAILED";
  return "unknown";
}

function statusState(s) {
  if (s === "success") return "SUCCESSFUL";
  if (s === "pending") return "INPROGRESS";
  if (s === "failure" || s === "error") return "FAILED";
  return "unknown";
}

/**
 * A commit's builds: its check runs and its legacy commit statuses together, worst state first
 * (FAILED, then in progress, then successful — an unknown state counts as in progress, never green).
 * Null when it has neither. Same shape as Bitbucket's BuildSummary.
 */
function normalizeBuildStatus({ checkRuns, status }) {
  const builds = [];
  for (const run of Array.isArray(checkRuns?.check_runs) ? checkRuns.check_runs : []) {
    if (!isObject(run)) continue;
    builds.push({
      state: checkRunState(run),
      key: num(run.id) !== null ? String(run.id) : null,
      name: str(run.name),
      url: str(run.html_url),
      at: ms(run.completed_at) ?? ms(run.started_at),
    });
  }
  for (const s of Array.isArray(status?.statuses) ? status.statuses : []) {
    if (!isObject(s)) continue;
    builds.push({ state: statusState(s.state), key: str(s.context), name: str(s.context), url: str(s.target_url), at: ms(s.updated_at) });
  }
  if (builds.length === 0) return null;
  const counts = { SUCCESSFUL: 0, FAILED: 0, INPROGRESS: 0, unknown: 0 };
  for (const b of builds) counts[b.state] += 1;
  const state = counts.FAILED > 0 ? "FAILED" : counts.INPROGRESS + counts.unknown > 0 ? "INPROGRESS" : "SUCCESSFUL";
  return { state, counts, builds: builds.slice(0, 10) };
}

// ---- Dashboard (GraphQL search) ----

const REVIEW_STATUS = { APPROVED: "APPROVED", CHANGES_REQUESTED: "NEEDS_WORK" };

function normalizeDashboardPullRequest(node) {
  if (!isObject(node) || num(node.number) === null) return null;
  const repository = repoRef(node.repository);
  if (!repository.projectKey || !repository.slug) return null;
  const reviewers = new Map();
  const requests = isObject(node.reviewRequests) && Array.isArray(node.reviewRequests.nodes) ? node.reviewRequests.nodes : [];
  for (const r of requests) {
    const name = isObject(r) && isObject(r.requestedReviewer) ? login(r.requestedReviewer) : null;
    if (name) reviewers.set(name, "UNAPPROVED");
  }
  const reviews = isObject(node.latestReviews) && Array.isArray(node.latestReviews.nodes) ? node.latestReviews.nodes : [];
  for (const r of reviews) {
    const name = isObject(r) ? login(r.author) : null;
    if (name) reviewers.set(name, REVIEW_STATUS[r.state] || "UNAPPROVED");
  }
  const list = [...reviewers.entries()].map(([name, status]) => ({ name, status }));
  return {
    id: node.number,
    title: str(node.title),
    state: "OPEN",
    fromBranch: str(node.headRefName),
    fromSha: str(node.headRefOid),
    toBranch: str(node.baseRefName),
    url: str(node.url),
    updatedAt: ms(node.updatedAt),
    project: repository.projectKey,
    repo: repository.slug,
    toSha: str(node.baseRefOid),
    fromRepo: repoRef(node.headRepository),
    authorSlug: login(node.author),
    reviewers: list,
    approvals: list.filter((r) => r.status === "APPROVED").length,
  };
}

/** The open PRs of a GraphQL search result (`data.search.nodes`); anything that isn't a usable PR is dropped. */
function normalizeDashboard(raw) {
  const nodes = raw?.data?.search?.nodes;
  return (Array.isArray(nodes) ? nodes : []).map(normalizeDashboardPullRequest).filter(Boolean);
}

module.exports = {
  normalizePullRequest,
  normalizePullRequestSummary,
  normalizePullRequestList,
  normalizeMergeStatus,
  normalizeRepository,
  normalizeDefaultBranch,
  normalizeReviewThread,
  normalizeReviewThreads,
  normalizeBuildStatus,
  normalizeDashboardPullRequest,
  normalizeDashboard,
};
