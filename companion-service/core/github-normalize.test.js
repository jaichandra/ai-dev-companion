// core/github-normalize.js against hand-written GitHub responses (core/fixtures/github).
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const n = require("./github-normalize.js");
const addressPlan = require("../features/address-review-comments/plan.js");

const fx = (name) => JSON.parse(fs.readFileSync(path.join(__dirname, "fixtures", "github", name), "utf8"));

// ---- pull requests ----

test("a PR maps onto the normalized shape: branches, shas, repos, lower-cased author, comment count", () => {
  const pr = n.normalizePullRequest(fx("pull-request.json"));
  assert.deepEqual(pr, {
    id: 42,
    version: null,
    title: "Fix the flaky login test",
    state: "OPEN",
    fromBranch: "fix/login-test",
    fromSha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    fromRepo: { projectKey: "Octo", slug: "hello" },
    toBranch: "main",
    toSha: "0f1e2d3c4b5a69788796a5b4c3d2e1f001122334",
    toRepo: { projectKey: "Octo", slug: "hello" },
    authorSlug: "hubot",
    commentCount: 8,
  });
});

test("state: merged, closed without merging, and open", () => {
  assert.equal(n.normalizePullRequest(fx("pull-request-merged.json")).state, "MERGED");
  assert.equal(n.normalizePullRequest(fx("pull-request-deleted-fork.json")).state, "DECLINED");
  assert.equal(n.normalizePullRequest(fx("pull-request.json")).state, "OPEN");
  assert.equal(n.normalizePullRequest({ number: 1, state: "weird" }).state, null);
});

test("a fork PR names the fork as its source repository, a deleted fork leaves it empty", () => {
  assert.deepEqual(n.normalizePullRequest(fx("pull-request-fork.json")).fromRepo, { projectKey: "contributor", slug: "hello" });
  assert.deepEqual(n.normalizePullRequest(fx("pull-request-deleted-fork.json")).fromRepo, { projectKey: null, slug: null });
});

test("a fork PR is refused by address-review-comments' fork check, a same-repo PR passes it", () => {
  const payload = { project: "octo", repo: "hello" };
  const same = n.normalizePullRequest(fx("pull-request.json"));
  const fork = n.normalizePullRequest(fx("pull-request-fork.json"));
  assert.equal(addressPlan.pullRequestRepoError(same, payload), null);
  assert.match(String(addressPlan.pullRequestRepoError(fork, payload)), /fork|repository/i);
});

test("junk input never throws: it gives null, or null fields", () => {
  for (const junk of [null, undefined, 5, "x", [], {}]) {
    assert.doesNotThrow(() => n.normalizePullRequest(junk));
  }
  assert.equal(n.normalizePullRequest(null), null);
  assert.equal(n.normalizePullRequest({}).id, null);
  assert.equal(n.normalizePullRequestSummary({ title: "no number" }), null);
});

test("a PR list keeps each PR's address and last update (ms), and drops entries without a number", () => {
  const list = n.normalizePullRequestList(fx("pulls-list.json"));
  assert.deepEqual(list.map((p) => p.id), [42, 40]);
  assert.equal(list[0].url, "https://github.com/Octo/hello/pull/42");
  assert.equal(list[0].updatedAt, Date.parse("2026-09-30T12:34:56Z"));
  assert.equal(list[1].state, "MERGED");
  assert.deepEqual(n.normalizePullRequestList("nope"), []);
});

// ---- merge status ----

test("merge status: a conflicted PR, a clean one, one still being computed, and a blocked one", () => {
  assert.deepEqual(n.normalizeMergeStatus(fx("pull-request.json")), { conflicted: true, canMerge: false, outcome: "DIRTY" });
  assert.deepEqual(n.normalizeMergeStatus({ mergeable: true, mergeable_state: "clean" }), { conflicted: false, canMerge: true, outcome: "CLEAN" });
  assert.deepEqual(n.normalizeMergeStatus(fx("pull-request-fork.json")), { conflicted: null, canMerge: null, outcome: "UNKNOWN" });
  assert.deepEqual(n.normalizeMergeStatus({ mergeable: true, mergeable_state: "blocked" }), { conflicted: false, canMerge: false, outcome: "BLOCKED" });
  assert.deepEqual(n.normalizeMergeStatus(null), { conflicted: null, canMerge: null, outcome: null });
});

test("repository and default branch", () => {
  assert.deepEqual(n.normalizeRepository(fx("repo.json")), { id: 101, slug: "hello", projectKey: "Octo" });
  assert.equal(n.normalizeRepository({}), null);
  assert.equal(n.normalizeDefaultBranch(fx("repo.json")), "main");
  assert.equal(n.normalizeDefaultBranch(null), null);
});

// ---- review threads ----

test("review threads become comments: the first comment is the root, the rest are its replies", () => {
  const comments = n.normalizeReviewThreads([fx("review-threads-page1.json")]);
  const root = comments.find((c) => c.id === 1001);
  assert.equal(root.text, "Please handle the empty token.");
  assert.equal(root.authorSlug, "reviewer-one");
  assert.equal(root.authorName, "Reviewer-One");
  assert.equal(root.createdAt, Date.parse("2026-09-29T10:00:00Z"));
  assert.equal(root.severity, "NORMAL");
  assert.equal(root.state, "OPEN");
  assert.equal(root.threadResolved, false);
  assert.deepEqual(root.anchor, { path: "src/login.js", line: 42, lineType: "unknown", fileType: "TO", diffType: "EFFECTIVE", orphaned: false });
  assert.deepEqual(root.replies.map((r) => [r.id, r.authorSlug]), [[1002, "hubot"]]);
  assert.equal(root.replies[0].anchor, null);
});

test("a resolved thread says so, and an outdated one is orphaned", () => {
  const comments = n.normalizeReviewThreads([fx("review-threads-page1.json")]);
  const resolved = comments.find((c) => c.id === 1003);
  assert.equal(resolved.state, "RESOLVED");
  assert.equal(resolved.threadResolved, true);
  const outdated = comments.find((c) => c.id === 1004);
  assert.equal(outdated.anchor.orphaned, true);
  assert.equal(outdated.anchor.line, 12, "falls back to the original line when the current one is gone");
});

test("a comment on the removed side is marked as such", () => {
  const removed = n.normalizeReviewThreads([fx("review-threads-page2.json")]).find((c) => c.id === 1005);
  assert.equal(removed.anchor.lineType, "REMOVED");
  assert.equal(removed.anchor.fileType, "FROM");
});

test("pages are merged, a thread seen on two pages counts once, and empty threads are dropped", () => {
  const comments = n.normalizeReviewThreads([fx("review-threads-page1.json"), fx("review-threads-page2.json")]);
  assert.deepEqual(comments.map((c) => c.id).sort(), [1001, 1003, 1004, 1005]);
  assert.deepEqual(n.normalizeReviewThreads([]), []);
  assert.deepEqual(n.normalizeReviewThreads([{ errors: [{ message: "x" }] }]), []);
});

test("what address-review-comments treats as open: unresolved, current, not on the removed side", () => {
  const comments = n.normalizeReviewThreads([fx("review-threads-page1.json"), fx("review-threads-page2.json")]);
  const open = addressPlan.collectOpenComments(comments, "hubot");
  assert.deepEqual(open.map((c) => c.id), [1001], "1003 is resolved, 1004 is outdated, 1005 is on removed code");
});

// ---- builds ----

test("builds: check runs and commit statuses together, the worst state wins", () => {
  const summary = n.normalizeBuildStatus({ checkRuns: fx("check-runs.json"), status: fx("status.json") });
  assert.equal(summary.state, "FAILED");
  assert.deepEqual(summary.counts, { SUCCESSFUL: 2, FAILED: 1, INPROGRESS: 2, unknown: 0 });
  const lint = summary.builds.find((b) => b.name === "lint");
  assert.deepEqual(lint, { state: "FAILED", key: "12", name: "lint", url: "https://github.com/Octo/hello/runs/12", at: Date.parse("2026-09-30T10:02:00Z") });
});

test("builds: in progress beats successful, and an unknown state is never reported green", () => {
  const ok = { check_runs: [{ id: 1, name: "a", status: "completed", conclusion: "success" }] };
  assert.equal(n.normalizeBuildStatus({ checkRuns: ok, status: null }).state, "SUCCESSFUL");
  const running = { check_runs: [...ok.check_runs, { id: 2, name: "b", status: "queued", conclusion: null }] };
  assert.equal(n.normalizeBuildStatus({ checkRuns: running, status: null }).state, "INPROGRESS");
  const odd = { check_runs: [...ok.check_runs, { id: 3, name: "c", status: "completed", conclusion: "something-new" }] };
  const summary = n.normalizeBuildStatus({ checkRuns: odd, status: null });
  assert.equal(summary.state, "INPROGRESS");
  assert.equal(summary.counts.unknown, 1);
});

test("builds: none at all is null, and at most ten are listed", () => {
  assert.equal(n.normalizeBuildStatus({ checkRuns: null, status: null }), null);
  assert.equal(n.normalizeBuildStatus({ checkRuns: { check_runs: [] }, status: { statuses: [] } }), null);
  const many = { check_runs: Array.from({ length: 15 }, (_, i) => ({ id: i, name: `j${i}`, status: "completed", conclusion: "success" })) };
  const summary = n.normalizeBuildStatus({ checkRuns: many, status: null });
  assert.equal(summary.builds.length, 10);
  assert.equal(summary.counts.SUCCESSFUL, 15);
});

// ---- dashboard ----

test("dashboard PRs carry their repository, shas, source repo, reviewers and approvals", () => {
  const prs = n.normalizeDashboard(fx("dashboard.json"));
  assert.deepEqual(prs.map((p) => p.id), [7, 8, 10], "entries without a repository or a number are dropped");
  const first = prs[0];
  assert.equal(first.project, "Octo");
  assert.equal(first.repo, "hello");
  assert.equal(first.state, "OPEN");
  assert.equal(first.fromBranch, "feat/retries");
  assert.equal(first.toBranch, "main");
  assert.equal(first.fromSha, "e".repeat(40));
  assert.equal(first.toSha, "f".repeat(40));
  assert.equal(first.authorSlug, "hubot");
  assert.equal(first.url, "https://github.com/Octo/hello/pull/7");
  assert.deepEqual(first.fromRepo, { projectKey: "Octo", slug: "hello" });
  assert.equal(first.approvals, 1);
  assert.deepEqual(
    Object.fromEntries(first.reviewers.map((r) => [r.name, r.status])),
    { "reviewer-three": "UNAPPROVED", "reviewer-one": "APPROVED", "reviewer-two": "NEEDS_WORK" },
  );
});

test("dashboard: a fork shows as the source repo, and a team review request is skipped", () => {
  const prs = n.normalizeDashboard(fx("dashboard.json"));
  assert.deepEqual(prs.find((p) => p.id === 8).fromRepo, { projectKey: "someone", slug: "hello" });
  assert.deepEqual(prs.find((p) => p.id === 10).reviewers, [{ name: "reviewer-one", status: "UNAPPROVED" }]);
  assert.deepEqual(n.normalizeDashboard(null), []);
  assert.deepEqual(n.normalizeDashboard({ data: { search: { nodes: "no" } } }), []);
});
