// GitHub's REST and GraphQL addresses and queries for the git host provider (core/github.ts). Pure:
// builds paths and bodies, and refuses anything unsafe before a request exists — the owner, repo and
// branch names reach a URL path, a search query and (from the clone) a git command line.
//
// The web address (https://github.com, or https://ghe.example.com) and the API address differ:
//   github.com        REST https://api.github.com            GraphQL https://api.github.com/graphql
//   GitHub Enterprise REST https://<host>/api/v3             GraphQL https://<host>/api/graphql
const { isSafeRepoSegment } = require("./prereqs.js");
const { isSafeBranchName } = require("./bitbucket-endpoints.js");

const API_VERSION = "2022-11-28";
/** Headers every API call carries. */
const API_HEADERS = { Accept: "application/vnd.github+json", "X-GitHub-Api-Version": API_VERSION };

/** The REST and GraphQL base URLs for a web base URL such as "https://github.com" (no trailing slash). */
function apiBases(webBaseUrl) {
  const url = new URL(webBaseUrl);
  if (url.hostname.toLowerCase() === "github.com" || url.hostname.toLowerCase() === "www.github.com") {
    return { rest: "https://api.github.com", graphql: "https://api.github.com/graphql" };
  }
  return { rest: `${url.origin}/api/v3`, graphql: `${url.origin}/api/graphql` };
}

function assertRepo(owner, repo) {
  if (!isSafeRepoSegment(owner) || !isSafeRepoSegment(repo)) {
    throw new Error(`Not a safe GitHub owner/repo: ${String(owner).slice(0, 40)}/${String(repo).slice(0, 40)}`);
  }
}

function assertPrNumber(n) {
  if (!Number.isInteger(n) || n < 1) throw new Error(`Not a pull request number: ${String(n).slice(0, 20)}`);
}

function assertBranch(branch) {
  if (!isSafeBranchName(branch)) throw new Error(`Not a safe branch name: ${String(branch).slice(0, 80)}`);
}

const enc = encodeURIComponent;
const repoBase = (owner, repo) => {
  assertRepo(owner, repo);
  return `/repos/${enc(owner)}/${enc(repo)}`;
};

const userPath = () => "/user";
const repoPath = (owner, repo) => repoBase(owner, repo);
const pullPath = (owner, repo, n) => {
  assertPrNumber(n);
  return `${repoBase(owner, repo)}/pulls/${n}`;
};
const createPullPath = (owner, repo) => `${repoBase(owner, repo)}/pulls`;
const requestedReviewersPath = (owner, repo, n) => `${pullPath(owner, repo, n)}/requested_reviewers`;
const replyPath = (owner, repo, n, commentId) => {
  if (!Number.isInteger(commentId) || commentId < 1) throw new Error("Not a comment id");
  return `${pullPath(owner, repo, n)}/comments/${commentId}/replies`;
};

/** Pull requests whose head is `branch` of the same repository, newest first. */
function branchPullsPath(owner, repo, branch, { limit = 10 } = {}) {
  assertBranch(branch);
  const head = enc(`${owner}:${branch}`);
  return `${repoBase(owner, repo)}/pulls?head=${head}&state=all&sort=updated&direction=desc&per_page=${Math.max(1, Math.min(limit, 100))}`;
}

function checkRunsPath(owner, repo, sha) {
  if (!/^[0-9a-f]{7,40}$/i.test(sha)) throw new Error("Not a commit hash");
  return `${repoBase(owner, repo)}/commits/${sha}/check-runs?per_page=100`;
}
function combinedStatusPath(owner, repo, sha) {
  if (!/^[0-9a-f]{7,40}$/i.test(sha)) throw new Error("Not a commit hash");
  return `${repoBase(owner, repo)}/commits/${sha}/status`;
}

/** The body of the create-pull-request call. Reviewers are added by a separate call afterwards. */
function createPullBody({ owner, repo, title, description, fromBranch, toBranch }) {
  assertRepo(owner, repo);
  assertBranch(fromBranch);
  assertBranch(toBranch);
  return { title: String(title), body: String(description || ""), head: fromBranch, base: toBranch };
}

// ---- GraphQL ----

/** Review threads (with their comments) of one pull request. REST can't say whether a thread is resolved. */
const REVIEW_THREADS_QUERY = `query ReviewThreads($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          isResolved
          isOutdated
          path
          line
          originalLine
          diffSide
          comments(first: 100) {
            nodes {
              databaseId
              body
              createdAt
              author { login }
              replyTo { databaseId }
            }
          }
        }
      }
    }
  }
}`;

const PULL_REQUEST_FIELDS = `number title url updatedAt isDraft headRefName baseRefName headRefOid baseRefOid
  repository { name owner { login } }
  headRepository { name owner { login } }
  author { login }
  latestReviews(first: 20) { nodes { state author { login } } }
  reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } }`;

/** Open pull requests matching a search (see dashboardSearch). */
const DASHBOARD_QUERY = `query Dashboard($search: String!) {
  search(query: $search, type: ISSUE, first: 30) {
    nodes { ... on PullRequest { ${PULL_REQUEST_FIELDS} } }
  }
}`;

/** The search string for your open PRs (AUTHOR) or the ones waiting on your review (REVIEWER). */
function dashboardSearch(role, login) {
  if (!/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/.test(login)) throw new Error("Not a GitHub login");
  const who = role === "REVIEWER" ? `review-requested:${login}` : `author:${login}`;
  return `is:pr is:open archived:false ${who}`;
}

module.exports = {
  API_HEADERS,
  API_VERSION,
  apiBases,
  assertRepo,
  userPath,
  repoPath,
  pullPath,
  createPullPath,
  requestedReviewersPath,
  replyPath,
  branchPullsPath,
  checkRunsPath,
  combinedStatusPath,
  createPullBody,
  REVIEW_THREADS_QUERY,
  DASHBOARD_QUERY,
  dashboardSearch,
};
