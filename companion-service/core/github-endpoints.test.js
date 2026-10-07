// core/github-endpoints.js: addresses, bodies and queries, and what is refused before a request exists.
const test = require("node:test");
const assert = require("node:assert/strict");
const e = require("./github-endpoints.js");

test("github.com's API is api.github.com; an Enterprise host serves REST under /api/v3 and GraphQL under /api/graphql", () => {
  assert.deepEqual(e.apiBases("https://github.com"), { rest: "https://api.github.com", graphql: "https://api.github.com/graphql" });
  assert.deepEqual(e.apiBases("https://GitHub.com"), { rest: "https://api.github.com", graphql: "https://api.github.com/graphql" });
  assert.deepEqual(e.apiBases("https://ghe.example.com"), { rest: "https://ghe.example.com/api/v3", graphql: "https://ghe.example.com/api/graphql" });
  assert.deepEqual(e.apiBases("https://ghe.example.com:8443"), { rest: "https://ghe.example.com:8443/api/v3", graphql: "https://ghe.example.com:8443/api/graphql" });
});

test("paths are pinned to the repository, encode their segments, and take a repo like .github", () => {
  assert.equal(e.pullPath("octo", "hello", 7), "/repos/octo/hello/pulls/7");
  assert.equal(e.pullPath("octo", ".github", 7), "/repos/octo/.github/pulls/7");
  assert.equal(e.repoPath("octo", "hello"), "/repos/octo/hello");
  assert.equal(e.requestedReviewersPath("o", "r", 3), "/repos/o/r/pulls/3/requested_reviewers");
  assert.equal(e.replyPath("o", "r", 3, 99), "/repos/o/r/pulls/3/comments/99/replies");
  assert.equal(e.userPath(), "/user");
});

test("a branch's PRs are asked for by owner:branch, newest first, capped", () => {
  assert.equal(e.branchPullsPath("octo", "hello", "feat/x"), "/repos/octo/hello/pulls?head=octo%3Afeat%2Fx&state=all&sort=updated&direction=desc&per_page=10");
  assert.match(e.branchPullsPath("octo", "hello", "b", { limit: 500 }), /per_page=100$/);
});

test("a commit's builds are two reads: check runs and the combined status", () => {
  const sha = "a".repeat(40);
  assert.equal(e.checkRunsPath("o", "r", sha), `/repos/o/r/commits/${sha}/check-runs?per_page=100`);
  assert.equal(e.combinedStatusPath("o", "r", sha), `/repos/o/r/commits/${sha}/status`);
});

test("unsafe owners, repos, numbers, branches, hashes and logins are refused before any request", () => {
  assert.throws(() => e.pullPath("o", "../x", 1), /Not a safe GitHub owner\/repo/);
  assert.throws(() => e.pullPath("o/x", "r", 1), /Not a safe GitHub owner\/repo/);
  assert.throws(() => e.pullPath("o", "r", 0), /Not a pull request number/);
  assert.throws(() => e.pullPath("o", "r", "1"), /Not a pull request number/);
  assert.throws(() => e.branchPullsPath("o", "r", "-rf"), /Not a safe branch name/);
  assert.throws(() => e.checkRunsPath("o", "r", "zzz"), /Not a commit hash/);
  assert.throws(() => e.replyPath("o", "r", 1, "x"), /Not a comment id/);
  assert.throws(() => e.dashboardSearch("AUTHOR", "a b"), /Not a GitHub login/);
  assert.throws(() => e.dashboardSearch("AUTHOR", "x is:private"), /Not a GitHub login/);
});

test("the create-PR body names the branches and leaves reviewers to the follow-up call", () => {
  assert.deepEqual(e.createPullBody({ owner: "o", repo: "r", title: "T", description: "D", fromBranch: "feat/x", toBranch: "main" }), {
    title: "T",
    body: "D",
    head: "feat/x",
    base: "main",
  });
  assert.throws(() => e.createPullBody({ owner: "o", repo: "r", title: "T", description: "", fromBranch: "--evil", toBranch: "main" }), /Not a safe branch name/);
});

test("the dashboard search is your PRs or the ones waiting on you, open and not archived", () => {
  assert.equal(e.dashboardSearch("AUTHOR", "octo-cat"), "is:pr is:open archived:false author:octo-cat");
  assert.equal(e.dashboardSearch("REVIEWER", "octo-cat"), "is:pr is:open archived:false review-requested:octo-cat");
});

test("the API headers ask for GitHub's JSON type and pin a version", () => {
  assert.equal(e.API_HEADERS.Accept, "application/vnd.github+json");
  assert.match(e.API_HEADERS["X-GitHub-Api-Version"], /^\d{4}-\d{2}-\d{2}$/);
});

test("the GraphQL queries ask for what the normalizers read", () => {
  for (const field of ["isResolved", "isOutdated", "diffSide", "databaseId", "replyTo", "hasNextPage", "endCursor"]) {
    assert.ok(e.REVIEW_THREADS_QUERY.includes(field), field);
  }
  for (const field of ["headRefOid", "baseRefOid", "latestReviews", "reviewRequests", "headRepository"]) {
    assert.ok(e.DASHBOARD_QUERY.includes(field), field);
  }
});
