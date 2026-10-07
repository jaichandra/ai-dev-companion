// core/github.ts and the GitHub provider, from the build (npm run build first), with global fetch stubbed (no
// network) and HOME in a temp dir first, so the token store is a throwaway.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "github-test-home-"));
const github = require("../dist/core/github.js");
const providers = require("../dist/core/providers.js");
const credentials = require("../dist/core/credentials.js");
const { AuthSetupError, HttpStatusError } = require("../dist/core/atlassian.js");

const fx = (name) => JSON.parse(fs.readFileSync(path.join(__dirname, "fixtures", "github", name), "utf8"));
const TOKEN = "ghp_testtoken";
// The provider serves whichever site the profile gives it (its settings and token live under that site's id).
// The framework's placeholder profile has Jira, Jenkins and Bitbucket sites only, so these tests serve GitHub
// through the "bitbucket" one; the example profile (a real "github" site) is exercised by its own test.
const SITE = "bitbucket";
const config = (extra = {}) => ({ bitbucket: { baseUrl: "https://github.com" }, ...extra });
const json = (body, status = 200, headers = {}) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });

/** Runs `fn` with fetch answering from `handler(url, init)`; returns what was requested. */
async function withFetch(handler, fn) {
  const real = globalThis.fetch;
  const requests = [];
  globalThis.fetch = async (url, init = {}) => {
    const request = { url: String(url), method: init.method || "GET", headers: init.headers || {}, body: init.body ? JSON.parse(init.body) : undefined };
    requests.push(request);
    return handler(request);
  };
  try {
    return { result: await fn(), requests };
  } finally {
    globalThis.fetch = real;
  }
}
const withToken = (fn) => async () => {
  credentials.setToken("bitbucket.apiToken", TOKEN);
  try {
    await fn();
  } finally {
    credentials.setToken("bitbucket.apiToken", null);
  }
};

// ---- the site ----

test("github.com: web address, REST and GraphQL on api.github.com, a token-only site with GitHub's headers", withToken(() => {
  const site = github.githubSite(config(), SITE);
  assert.equal(site.web, "https://github.com");
  assert.equal(site.rest.baseUrl, "https://api.github.com");
  assert.equal(site.graphql.baseUrl, "https://api.github.com/graphql");
  assert.equal(site.rest.tokenOnly, true);
  assert.equal(site.rest.apiToken, TOKEN);
  assert.equal(site.rest.headers.Accept, "application/vnd.github+json");
}));

test("an Enterprise host from config.json, given without a scheme or with a trailing slash", () => {
  const site = github.githubSite(config({ bitbucket: { baseUrl: "ghe.example.com/" } }), SITE);
  assert.equal(site.web, "https://ghe.example.com");
  assert.equal(site.rest.baseUrl, "https://ghe.example.com/api/v3");
  assert.equal(site.graphql.baseUrl, "https://ghe.example.com/api/graphql");
});

// ---- auth ----

test("a call carries the token and GitHub's headers, and never a cookie, even when the page relayed one", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { requests } = await withFetch(() => json(fx("user.json")), () => github.whoAmI(site, { cookie: "user_session=abc", origin: "https://github.com" }));
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, "https://api.github.com/user");
  assert.equal(requests[0].headers.Authorization, `Bearer ${TOKEN}`);
  assert.equal(requests[0].headers.Accept, "application/vnd.github+json");
  assert.match(requests[0].headers["X-GitHub-Api-Version"], /^\d{4}-/);
  assert.equal(requests[0].headers.Cookie, undefined);
}));

test("whoAmI is the lower-cased login", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result } = await withFetch(() => json(fx("user.json")), () => github.whoAmI(site, {}));
  assert.equal(result, "hubot");
}));

test("with no token saved the call says to create one, and makes no request", async () => {
  const site = github.githubSite(config(), SITE);
  const { requests } = await withFetch(() => json({}), async () => {
    await assert.rejects(() => github.whoAmI(site, {}), (err) => err instanceof AuthSetupError && /personal access token/.test(err.message));
  });
  assert.equal(requests.length, 0);
});

test("a 401 means the token was rejected", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  await withFetch(() => json({ message: "Bad credentials" }, 401), async () => {
    await assert.rejects(() => github.whoAmI(site, {}), (err) => err instanceof AuthSetupError && /rejected the saved API token \(bitbucket\.apiToken\)/.test(err.message));
  });
}));

test("a 403 with no calls left is a rate limit, not a bad token", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const reset = String(Math.floor(Date.parse("2026-10-06T15:30:00Z") / 1000));
  await withFetch(() => json({ message: "API rate limit exceeded" }, 403, { "x-ratelimit-remaining": "0", "x-ratelimit-reset": reset }), async () => {
    await assert.rejects(
      () => github.whoAmI(site, {}),
      (err) => err instanceof HttpStatusError && err.status === 403 && /rate limit reached \(resets at 15:30 UTC\)/.test(err.message),
    );
  });
}));

test("a secondary rate limit (429 with retry-after) says when to retry", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  await withFetch(() => json({ message: "slow down" }, 429, { "retry-after": "30" }), async () => {
    await assert.rejects(() => github.whoAmI(site, {}), (err) => err instanceof HttpStatusError && /retry in 30s/.test(err.message));
  });
}));

test("any other 403 is a missing permission: GitHub's own message comes through, not 'bad token'", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  await withFetch(() => json({ message: "Resource not accessible by personal access token" }, 403), async () => {
    await assert.rejects(
      () => github.getPullRequest(site, {}, "o", "r", 1),
      (err) => err instanceof HttpStatusError && !(err instanceof AuthSetupError) && /Resource not accessible by personal access token/.test(err.message),
    );
  });
}));

// ---- reading ----

test("getPullRequest reads the PR and returns the normalized shape", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result, requests } = await withFetch(() => json(fx("pull-request.json")), () => github.getPullRequest(site, {}, "Octo", "hello", 42));
  assert.equal(requests[0].url, "https://api.github.com/repos/Octo/hello/pulls/42");
  assert.equal(result.fromBranch, "fix/login-test");
  assert.equal(result.state, "OPEN");
}));

test("unsafe names are refused before any request", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { requests } = await withFetch(() => json({}), async () => {
    await assert.rejects(() => github.getPullRequest(site, {}, "o", "../x", 1), /Not a safe GitHub owner\/repo/);
    await assert.rejects(() => github.listBranchPullRequests(site, {}, "o", "r", "-bad"), /Not a safe branch name/);
    await assert.rejects(() => github.getCommitBuildStatus(site, {}, "o", "r", "nothex"), /Not a commit hash/);
    await assert.rejects(() => github.listActivities(site, {}, "o", "r", 0), /Not a pull request number/);
  });
  assert.equal(requests.length, 0);
}));

test("review threads: one GraphQL call per page, following the cursor, normalized", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const pages = [fx("review-threads-page1.json"), fx("review-threads-page2.json")];
  const { result, requests } = await withFetch(() => json(pages.shift()), () => github.listActivities(site, {}, "Octo", "hello", 42));
  assert.equal(requests.length, 2);
  assert.equal(requests[0].url, "https://api.github.com/graphql");
  assert.equal(requests[0].method, "POST");
  assert.deepEqual(requests[0].body.variables, { owner: "Octo", name: "hello", number: 42, cursor: null });
  assert.equal(requests[1].body.variables.cursor, "CURSOR1");
  assert.deepEqual(result.map((c) => c.id).sort(), [1001, 1003, 1004, 1005]);
}));

test("a GraphQL error (a repository that isn't there) is an error, not an empty list", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  await withFetch(() => json({ data: null, errors: [{ message: "Could not resolve to a Repository with the name 'o/r'." }] }), async () => {
    await assert.rejects(() => github.listActivities(site, {}, "o", "r", 1), /Could not resolve to a Repository/);
  });
}));

test("an Enterprise host's GraphQL is under /api/graphql", withToken(async () => {
  const site = github.githubSite(config({ bitbucket: { baseUrl: "https://ghe.example.com" } }), SITE);
  const { requests } = await withFetch(() => json(fx("review-threads-page2.json")), () => github.listActivities(site, {}, "o", "r", 1));
  assert.equal(requests[0].url, "https://ghe.example.com/api/graphql");
}));

test("default branch: the repository's, or null when the repository isn't visible", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const ok = await withFetch(() => json(fx("repo.json")), () => github.getDefaultBranch(site, {}, "Octo", "hello"));
  assert.equal(ok.result, "main");
  const missing = await withFetch(() => json({ message: "Not Found" }, 404), () => github.getDefaultBranch(site, {}, "Octo", "hello"));
  assert.equal(missing.result, null);
}));

test("a branch's PRs: asked by owner:branch, normalized", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result, requests } = await withFetch(() => json(fx("pulls-list.json")), () => github.listBranchPullRequests(site, {}, "Octo", "hello", "fix/login-test"));
  assert.match(requests[0].url, /\/repos\/Octo\/hello\/pulls\?head=Octo%3Afix%2Flogin-test&state=all/);
  assert.deepEqual(result.map((p) => p.id), [42, 40]);
}));

test("builds: check runs and statuses, either of which may be missing (404)", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const sha = "a".repeat(40);
  const both = await withFetch((r) => json(r.url.includes("check-runs") ? fx("check-runs.json") : fx("status.json")), () => github.getCommitBuildStatus(site, {}, "o", "r", sha));
  assert.equal(both.result.state, "FAILED");
  assert.equal(both.requests.length, 2);
  const none = await withFetch(() => json({ message: "Not Found" }, 404), () => github.getCommitBuildStatus(site, {}, "o", "r", sha));
  assert.equal(none.result, null);
  const onlyStatus = await withFetch((r) => (r.url.includes("check-runs") ? json({ message: "Not Found" }, 404) : json(fx("status.json"))), () => github.getCommitBuildStatus(site, {}, "o", "r", sha));
  assert.equal(onlyStatus.result.state, "INPROGRESS");
}));

test("the dashboard asks who you are, then searches for that login", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result, requests } = await withFetch((r) => json(r.url.endsWith("/user") ? fx("user.json") : fx("dashboard.json")), () => github.listDashboardPullRequests(site, {}, "REVIEWER"));
  assert.equal(requests[0].url, "https://api.github.com/user");
  assert.equal(requests[1].body.variables.search, "is:pr is:open archived:false review-requested:hubot");
  assert.deepEqual(result.map((p) => p.id), [7, 8, 10]);
  const none = await withFetch(() => json({}), () => github.listDashboardPullRequests(site, {}, "AUTHOR"));
  assert.deepEqual(none.result, [], "no login, nothing to search for");
}));

// ---- merge status ----

test("merge status: an unknown answer is asked again, and a late answer is used", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const answers = [fx("pull-request-fork.json"), fx("pull-request.json")];
  const slept = [];
  const { result, requests } = await withFetch(() => json(answers.shift()), () => github.getMergeStatus(site, {}, "Octo", "hello", 42, async (ms) => slept.push(ms)));
  assert.equal(result.conflicted, true);
  assert.equal(requests.length, 2);
  assert.deepEqual(slept, [700]);
}));

test("merge status: still unknown after the retries stays unknown, never 'no conflict'", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result, requests } = await withFetch(() => json(fx("pull-request-fork.json")), () => github.getMergeStatus(site, {}, "Octo", "hello", 43, async () => {}));
  assert.equal(result.conflicted, null);
  assert.equal(requests.length, 3);
}));

test("merge status: a settled answer is not asked again", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { requests } = await withFetch(() => json({ ...fx("pull-request.json"), mergeable: true, mergeable_state: "clean" }), () => github.getMergeStatus(site, {}, "Octo", "hello", 42, async () => {}));
  assert.equal(requests.length, 1);
}));

// ---- writing ----

test("replyToComment posts into the thread of the comment and returns the new id", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result, requests } = await withFetch(() => json({ id: 5555 }, 201), () => github.replyToComment(site, {}, "Octo", "hello", 42, 1001, "Done."));
  assert.equal(requests[0].url, "https://api.github.com/repos/Octo/hello/pulls/42/comments/1001/replies");
  assert.equal(requests[0].method, "POST");
  assert.deepEqual(requests[0].body, { body: "Done." });
  assert.equal(result, 5555);
}));

test("createPullRequest opens the PR, then requests the reviewers", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const created = { ...fx("pull-request.json"), number: 77, html_url: "https://github.com/Octo/hello/pull/77" };
  const { result, requests } = await withFetch((r) => json(r.url.endsWith("/pulls") ? created : { ok: true }, 201), () =>
    github.createPullRequest(site, {}, { project: "Octo", repo: "hello", title: "T", description: "D", fromBranch: "feat/x", toBranch: "main", reviewers: ["alice", "bob"] }),
  );
  assert.equal(result.id, 77);
  assert.equal(result.url, "https://github.com/Octo/hello/pull/77");
  assert.equal(requests.length, 2);
  assert.deepEqual(requests[0].body, { title: "T", body: "D", head: "feat/x", base: "main" });
  assert.equal(requests[1].url, "https://api.github.com/repos/Octo/hello/pulls/77/requested_reviewers");
  assert.deepEqual(requests[1].body, { reviewers: ["alice", "bob"] });
}));

test("createPullRequest: a refused reviewer doesn't undo the PR, and no reviewers means no second call", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const created = { ...fx("pull-request.json"), number: 78, html_url: "https://github.com/Octo/hello/pull/78" };
  const quiet = console.warn;
  console.warn = () => {};
  try {
    const refused = await withFetch((r) => (r.url.endsWith("/pulls") ? json(created, 201) : json({ message: "Reviews may only be requested from collaborators" }, 422)), () =>
      github.createPullRequest(site, {}, { project: "Octo", repo: "hello", title: "T", description: "", fromBranch: "feat/x", toBranch: "main", reviewers: ["stranger"] }),
    );
    assert.equal(refused.result.id, 78);
    const none = await withFetch(() => json(created, 201), () =>
      github.createPullRequest(site, {}, { project: "Octo", repo: "hello", title: "T", description: "", fromBranch: "feat/x", toBranch: "main", reviewers: [] }),
    );
    assert.equal(none.requests.length, 1);
  } finally {
    console.warn = quiet;
  }
}));

test("createPullRequest refuses an unsafe branch before any request, and a reply that isn't a PR", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { requests } = await withFetch(() => json({}), async () => {
    await assert.rejects(() => github.createPullRequest(site, {}, { project: "o", repo: "r", title: "T", description: "", fromBranch: "--evil", toBranch: "main", reviewers: [] }), /Not a safe branch name/);
    await assert.rejects(() => github.createPullRequest(site, {}, { project: "o", repo: "r", title: "T", description: "", fromBranch: "a", toBranch: "main", reviewers: [] }), /didn't return the new pull request/);
  });
  assert.equal(requests.length, 1, "only the well-formed attempt was sent");
}));

test("repository id comes from the repository; default reviewers are always empty", withToken(async () => {
  const site = github.githubSite(config(), SITE);
  const { result } = await withFetch(() => json(fx("repo.json")), () => github.getRepositoryId(site, {}, "Octo", "hello"));
  assert.equal(result, 101);
  await withFetch(() => json({}), async () => {
    await assert.rejects(() => github.getRepositoryId(site, {}, "Octo", "hello"), /didn't return the repository/);
  });
  assert.deepEqual(await github.getDefaultReviewers(), []);
}));

// ---- the provider ----

test("the GitHub provider maps owner/repo/number onto the git-host interface", withToken(async () => {
  const p = providers.githubProvider(config(), SITE);
  assert.equal(p.id, "github");
  assert.equal(p.baseUrl(), "https://github.com");
  assert.equal(p.hasToken(), true);
  assert.equal(p.prUrl("Octo", "hello", 42), "https://github.com/Octo/hello/pull/42");
  assert.deepEqual([...p.capabilities].sort(), ["buildStatus", "dashboard", "mergeStatus"]);
  assert.ok(!p.capabilities.has("defaultReviewers"));
  assert.deepEqual(await p.getDefaultReviewers({}, "o", "r", {}), []);
}));

test("without a token the provider says so", () => {
  assert.equal(providers.githubProvider(config(), SITE).hasToken(), false);
});

test("an Enterprise provider's PR addresses use its own host", () => {
  const p = providers.githubProvider(config({ bitbucket: { baseUrl: "https://ghe.example.com" } }), SITE);
  assert.equal(p.prUrl("o", "r", 3), "https://ghe.example.com/o/r/pull/3");
});
