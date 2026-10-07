const test = require("node:test");
const assert = require("node:assert/strict");
const n = require("./bitbucket-normalize.js");
const e = require("./bitbucket-endpoints.js");

const RAW_PR = {
  id: 12,
  version: 3,
  title: "PROJ-7: fix login",
  state: "OPEN",
  updatedDate: 1700000000000,
  fromRef: { displayId: "bugfix/PROJ-7-login", latestCommit: "abc1234def", repository: { slug: "sample-app", project: { key: "ACME" } } },
  toRef: { displayId: "master", latestCommit: "0001112223", repository: { slug: "sample-app", project: { key: "ACME" } } },
  author: { user: { slug: "me" } },
  links: { self: [{ href: "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12" }] },
};

test("normalizePullRequestList keeps PRs with an id, adding url and updatedAt", () => {
  const list = n.normalizePullRequestList({ values: [RAW_PR, { title: "no id" }, "junk"], isLastPage: true });
  assert.equal(list.length, 1);
  assert.equal(list[0].id, 12);
  assert.equal(list[0].fromBranch, "bugfix/PROJ-7-login");
  assert.equal(list[0].fromSha, "abc1234def");
  assert.equal(list[0].url, "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12");
  assert.equal(list[0].updatedAt, 1700000000000);
  assert.deepEqual(n.normalizePullRequestList(null), []);
  assert.deepEqual(n.normalizePullRequestList({ values: "x" }), []);
  assert.equal(n.normalizePullRequestSummary({ id: 1 }).url, null);
});

test("normalizeBuildStatuses reports the worst state, and never green for an unknown one", () => {
  const b = (state) => ({ state, key: "k", name: "n", url: "https://ci/1", dateAdded: 5 });
  assert.equal(n.normalizeBuildStatuses({ values: [b("SUCCESSFUL"), b("FAILED")] }).state, "FAILED");
  assert.equal(n.normalizeBuildStatuses({ values: [b("SUCCESSFUL"), b("INPROGRESS")] }).state, "INPROGRESS");
  assert.equal(n.normalizeBuildStatuses({ values: [b("SUCCESSFUL"), b("WEIRD")] }).state, "INPROGRESS");
  assert.equal(n.normalizeBuildStatuses({ values: [b("SUCCESSFUL")] }).state, "SUCCESSFUL");
  const none = n.normalizeBuildStatuses({ values: [] });
  assert.equal(none.state, null);
  assert.deepEqual(none.counts, { SUCCESSFUL: 0, FAILED: 0, INPROGRESS: 0, unknown: 0 });
  assert.equal(n.normalizeBuildStatuses(undefined).state, null);
  assert.equal(n.normalizeBuildStatuses({ values: [b("FAILED")] }).builds[0].url, "https://ci/1");
});

test("normalizeDefaultBranch, normalizeRepository and normalizeDefaultReviewers read defensively", () => {
  assert.equal(n.normalizeDefaultBranch({ id: "refs/heads/master", displayId: "master" }), "master");
  assert.equal(n.normalizeDefaultBranch({ id: "refs/heads/main" }), "main");
  assert.equal(n.normalizeDefaultBranch({ id: "refs/tags/v1" }), null);
  assert.equal(n.normalizeDefaultBranch(null), null);
  assert.deepEqual(n.normalizeRepository({ id: 42, slug: "sample-app", project: { key: "ACME" } }), { id: 42, slug: "sample-app", projectKey: "ACME" });
  assert.equal(n.normalizeRepository({ slug: "sample-app" }), null);
  assert.deepEqual(
    n.normalizeDefaultReviewers([{ name: "ann" }, { slug: "bob" }, { name: "ann" }, { name: "cat", active: false }, 7]),
    ["ann", "bob"],
  );
  assert.deepEqual(n.normalizeDefaultReviewers({ values: [] }), []);
});

test("isSafeBranchName accepts ordinary branches and refuses option-, range- and ref-like names", () => {
  for (const ok of ["master", "bugfix/PROJ-7-login", "release/9.4", "feature/a_b"]) assert.ok(e.isSafeBranchName(ok), ok);
  for (const bad of ["-x", "a..b", "a//b", "a/", "a.", "a.lock", "a b", "a;b", "a@{1}", "", "x".repeat(201), 5]) {
    assert.ok(!e.isSafeBranchName(bad), String(bad));
  }
});

test("paths are pinned to /rest/api/1.0, encode segments and refuse unsafe input", () => {
  assert.deepEqual(e.defaultBranchPaths("ACME", "sample-app"), [
    "/rest/api/1.0/projects/ACME/repos/sample-app/default-branch",
    "/rest/api/1.0/projects/ACME/repos/sample-app/branches/default",
  ]);
  assert.equal(
    e.branchPullRequestsPath("ACME", "sample-app", "bugfix/PROJ-7-login"),
    "/rest/api/1.0/projects/ACME/repos/sample-app/pull-requests?state=ALL&direction=OUTGOING&at=refs%2Fheads%2Fbugfix%2FPROJ-7-login&order=NEWEST&limit=10",
  );
  assert.deepEqual(e.commitBuildsPaths("ACME", "sample-app", "abc1234"), ["/rest/build-status/1.0/commits/abc1234"]);
  assert.equal(
    e.defaultReviewersPath("ACME", "sample-app", { sourceRepoId: 4, targetRepoId: 4, sourceBranch: "b", targetBranch: "master" }),
    "/rest/default-reviewers/1.0/projects/ACME/repos/sample-app/reviewers?sourceRepoId=4&targetRepoId=4&sourceRefId=refs%2Fheads%2Fb&targetRefId=refs%2Fheads%2Fmaster",
  );
  assert.equal(e.createPullRequestPath("ACME", "sample-app"), "/rest/api/1.0/projects/ACME/repos/sample-app/pull-requests");
  assert.throws(() => e.repoPath("ACME", "../x"));
  assert.throws(() => e.branchPullRequestsPath("ACME", "sample-app", "-rf"));
  assert.throws(() => e.commitBuildsPaths("ACME", "sample-app", "HEAD;rm"));
  assert.throws(() => e.defaultReviewersPath("ACME", "sample-app", { sourceRepoId: "4", targetRepoId: 4, sourceBranch: "b", targetBranch: "m" }));
});

test("createPullRequestBody builds a same-repo PR with the given reviewers", () => {
  const body = e.createPullRequestBody({
    project: "ACME",
    repo: "sample-app",
    title: "T",
    description: "D",
    fromBranch: "bugfix/PROJ-7",
    toBranch: "master",
    reviewers: ["ann"],
  });
  assert.deepEqual(body, {
    title: "T",
    description: "D",
    fromRef: { id: "refs/heads/bugfix/PROJ-7", repository: { slug: "sample-app", project: { key: "ACME" } } },
    toRef: { id: "refs/heads/master", repository: { slug: "sample-app", project: { key: "ACME" } } },
    reviewers: [{ user: { name: "ann" } }],
  });
});

test("firstAvailable moves on only after a not-found, and throws anything else at once", async () => {
  const notFound = Object.assign(new Error("404"), { status: 404 });
  const isNotFound = (err) => err && err.status === 404;
  const seen = [];
  const r = await e.firstAvailable(["/a", "/b"], async (p) => {
    seen.push(p);
    if (p === "/a") throw notFound;
    return { ok: p };
  }, isNotFound);
  assert.deepEqual(r, { path: "/b", body: { ok: "/b" } });
  assert.deepEqual(seen, ["/a", "/b"]);

  const denied = Object.assign(new Error("401"), { status: 401 });
  const calls = [];
  await assert.rejects(
    e.firstAvailable(["/a", "/b"], async (p) => {
      calls.push(p);
      throw denied;
    }, isNotFound),
    /401/,
  );
  assert.deepEqual(calls, ["/a"]);

  await assert.rejects(e.firstAvailable(["/a", "/b"], async () => { throw notFound; }, isNotFound), /404/);
});
