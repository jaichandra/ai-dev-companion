const test = require("node:test");
const assert = require("node:assert/strict");
const w = require("./jira-workflow.js");
const { APP_SLUG } = require("./app-slug.js");

test("normalizeIssueBasics reads key, summary, type and status defensively", () => {
  assert.deepEqual(
    w.normalizeIssueBasics({
      key: "PROJ-7",
      fields: { summary: "Login fails", issuetype: { name: "Bug" }, status: { name: "Open" } },
    }),
    { key: "PROJ-7", summary: "Login fails", issueType: "Bug", status: "Open" },
  );
  assert.deepEqual(w.normalizeIssueBasics({}), { key: null, summary: null, issueType: null, status: null });
  assert.equal(w.normalizeIssueBasics("x"), null);
});

test("normalizeTransitions keeps entries with an id and stringifies it", () => {
  assert.deepEqual(
    w.normalizeTransitions({
      transitions: [{ id: "21", name: "Start Review", to: { name: "In Review" } }, { id: 31, name: "Done" }, { name: "no id" }],
    }),
    [
      { id: "21", name: "Start Review", to: "In Review" },
      { id: "31", name: "Done", to: null },
    ],
  );
  assert.deepEqual(w.normalizeTransitions(null), []);
});

test("pickTransition matches the name first, then the target status, ignoring case and spaces", () => {
  const ts = [
    { id: "1", name: "In Review", to: "Review" },
    { id: "2", name: "Send to review", to: "In Review" },
  ];
  assert.equal(w.pickTransition(ts, "in review").id, "1");
  assert.equal(w.pickTransition([ts[1]], " In Review ").id, "2");
  assert.equal(w.pickTransition(ts, "Done"), null);
  assert.equal(w.pickTransition(ts, ""), null);
});

test("remoteLinkBody builds an idempotent link and refuses non-https URLs", () => {
  const body = w.remoteLinkBody({ url: "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12", title: "PR #12: fix" });
  assert.equal(body.globalId, `${APP_SLUG}:pr:https://bb.example/projects/ACME/repos/sample-app/pull-requests/12`);
  assert.deepEqual(body.object, { url: "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12", title: "PR #12: fix" });
  assert.equal(w.remoteLinkBody({ url: "https://x/1", title: "t".repeat(400) }).object.title.length, 255);
  assert.throws(() => w.remoteLinkBody({ url: "javascript:alert(1)", title: "x" }));
});

test("remoteLinkBody refuses http:// and non-string addresses, and falls back to the URL for an empty title", () => {
  for (const url of ["http://bb.example/pr/1", "ftp://x/y", "//bb.example/x", "", undefined, null, 5, { toString: () => "https://x" }]) {
    assert.throws(() => w.remoteLinkBody({ url, title: "t" }), /https URL/, String(url));
  }
  const url = "https://bb.example/projects/ACME/repos/sample-app/pull-requests/1";
  for (const title of ["", "   ", undefined, null, 7]) assert.equal(w.remoteLinkBody({ url, title }).object.title, url);
  assert.equal(w.remoteLinkBody({ url, title: "  PR #1  " }).object.title, "PR #1");
});
