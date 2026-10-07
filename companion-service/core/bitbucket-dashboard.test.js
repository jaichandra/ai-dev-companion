const test = require("node:test");
const assert = require("node:assert/strict");
const n = require("./bitbucket-normalize.js");
const e = require("./bitbucket-endpoints.js");
const { normalizeSearchResults } = require("./jira-workflow.js");

const ref = (branch, sha) => ({ displayId: branch, latestCommit: sha, repository: { slug: "sample-app", project: { key: "CI" } } });
const RAW = {
  id: 12,
  title: "PROJ-7: fix login",
  state: "OPEN",
  fromRef: ref("bugfix/PROJ-7", "aaa111"),
  toRef: ref("master", "bbb222"),
  author: { user: { slug: "me" } },
  reviewers: [
    { user: { name: "ann" }, status: "APPROVED", approved: true },
    { user: { slug: "bob" }, status: "NEEDS_WORK" },
    { user: { name: "cat" }, status: "WEIRD" },
    { user: {} },
    "junk",
  ],
  links: { self: [{ href: "https://bb.example/projects/CI/repos/sample-app/pull-requests/12" }] },
};

test("dashboard paths are pinned to /rest/api/1.0 and refuse odd roles", () => {
  assert.equal(e.dashboardPullRequestsPath({ role: "AUTHOR" }), "/rest/api/1.0/dashboard/pull-requests?role=AUTHOR&state=OPEN&order=NEWEST&limit=50");
  assert.equal(
    e.dashboardPullRequestsPath({ role: "REVIEWER", participantStatus: "UNAPPROVED", limit: 500 }),
    "/rest/api/1.0/dashboard/pull-requests?role=REVIEWER&state=OPEN&order=NEWEST&limit=100&participantStatus=UNAPPROVED",
  );
  assert.throws(() => e.dashboardPullRequestsPath({ role: "ADMIN" }));
  assert.throws(() => e.dashboardPullRequestsPath({ role: "REVIEWER", participantStatus: "x" }));
  assert.equal(e.mergeStatusPath("CI", "sample-app", 12), "/rest/api/1.0/projects/CI/repos/sample-app/pull-requests/12/merge");
  assert.throws(() => e.mergeStatusPath("CI", "sample-app", 0));
  assert.throws(() => e.mergeStatusPath("CI", "../x", 1));
});

test("normalizeDashboardPullRequests adds the repo, reviewers and approvals, dropping junk", () => {
  const [pr, ...rest] = n.normalizeDashboardPullRequests({ values: [RAW, { id: 3 }, null] });
  assert.equal(rest.length, 0);
  assert.equal(pr.project, "CI");
  assert.equal(pr.repo, "sample-app");
  assert.equal(pr.fromSha, "aaa111");
  assert.equal(pr.toSha, "bbb222");
  assert.equal(pr.url, "https://bb.example/projects/CI/repos/sample-app/pull-requests/12");
  assert.deepEqual(pr.reviewers, [
    { name: "ann", status: "APPROVED" },
    { name: "bob", status: "NEEDS_WORK" },
    { name: "cat", status: "unknown" },
  ]);
  assert.equal(pr.approvals, 1);
  assert.deepEqual(n.normalizeDashboardPullRequests({}), []);
});

test("normalizeMergeStatus reads conflicted defensively", () => {
  assert.deepEqual(n.normalizeMergeStatus({ canMerge: false, conflicted: true, outcome: "CONFLICTED", vetoes: [] }), { conflicted: true, canMerge: false, outcome: "CONFLICTED" });
  assert.equal(n.normalizeMergeStatus({ outcome: "CONFLICTED" }).conflicted, true);
  assert.equal(n.normalizeMergeStatus({ outcome: "CLEAN" }).conflicted, null);
  assert.deepEqual(n.normalizeMergeStatus("x"), { conflicted: null, canMerge: null, outcome: null });
});

test("normalizeSearchResults keeps real issue keys and links them to the Jira base", () => {
  const out = normalizeSearchResults(
    {
      issues: [
        {
          key: "PROJ-7",
          fields: {
            summary: "Login broken",
            status: { name: "Open", statusCategory: { key: "new" } },
            issuetype: { name: "Bug" },
            priority: { name: "High" },
            updated: "2026-09-29T07:00:00.000+0000",
            description: "d".repeat(3000),
          },
        },
        { key: "not a key" },
        { fields: {} },
      ],
    },
    "https://jira.example/",
  );
  assert.equal(out.length, 1);
  assert.deepEqual({ ...out[0], description: out[0].description.length }, {
    key: "PROJ-7",
    summary: "Login broken",
    status: "Open",
    statusCategory: "new",
    issueType: "Bug",
    priority: "High",
    updated: "2026-09-29T07:00:00.000+0000",
    description: 2000,
    url: "https://jira.example/browse/PROJ-7",
  });
  assert.deepEqual(normalizeSearchResults(null, "x"), []);
});
