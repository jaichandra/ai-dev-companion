const test = require("node:test");
const assert = require("node:assert/strict");
const { buildDigest, digestNotification, MAX_ITEMS } = require("./plan.js");

const NOW = Date.parse("2026-09-29T08:30:00Z");
const pr = (id, over = {}) => ({
  id,
  title: `PR ${id}`,
  project: "CI",
  repo: "sample-app",
  url: `https://bb.example/projects/CI/repos/sample-app/pull-requests/${id}`,
  approvals: 0,
  reviewers: [],
  ...over,
});

test("an empty digest says nothing needs you and has no sections", () => {
  const d = buildDigest({ now: NOW });
  assert.equal(d.headline, "Nothing needs you right now.");
  assert.deepEqual(d.sections, []);
  assert.equal(d.generatedAt, "2026-09-29T08:30:00.000Z");
});

test("your PRs show conflicts, builds, needs-work and approvals, worst first", () => {
  const d = buildDigest({
    now: NOW,
    myPrs: [
      { pr: pr(1, { approvals: 2 }), conflicted: false, build: { state: "SUCCESSFUL" } },
      { pr: pr(2), conflicted: true, build: { state: "FAILED" } },
      { pr: pr(3, { reviewers: [{ name: "ann", status: "NEEDS_WORK" }] }), conflicted: false, build: null },
    ],
  });
  assert.equal(d.headline, "Needs you: 1 conflict, 1 failing build, 1 PR needing work.");
  assert.deepEqual(d.counts, { conflicts: 1, failingBuilds: 1, needsWork: 1, approved: 1, toReview: 0, tickets: 0, ready: 0, failedRuns: 0 });
  const items = d.sections[0].items;
  assert.equal(d.sections[0].id, "my-prs");
  assert.deepEqual(items.map((i) => i.tone), ["bad", "warn", "ok"]);
  assert.equal(items[0].text, "CI/sample-app #2 PR 2 — conflicts, build failing");
  assert.equal(items[1].text, "CI/sample-app #3 PR 3 — needs work (ann)");
  assert.equal(items[2].text, "CI/sample-app #1 PR 1 — approved by 2");
  assert.equal(items[0].url, "https://bb.example/projects/CI/repos/sample-app/pull-requests/2");
  assert.equal(items[0].flags, undefined, "internal flags aren't handed out");
});

test("reviews, tickets, pre-warmed results and notes each get a section; only https links survive", () => {
  const d = buildDigest({
    now: NOW,
    reviewPrs: [pr(9, { authorSlug: "bob", url: "javascript:alert(1)" })],
    tickets: [
      { key: "PROJ-7", summary: "Login broken", status: "Open", priority: "High", url: "https://jira.example/browse/PROJ-7" },
      { key: "PROJ-8", summary: "Typo", status: "In Progress", priority: "Low" },
    ],
    prewarmed: [
      { featureId: "resolve-conflict", status: "awaiting-approval", scopeKey: "bitbucket:CI/sample-app#2", summary: "Resolved 2 files" },
      { featureId: "analyze-issue", status: "failed", scopeKey: "jira:PROJ-7", error: "Claude exited" },
    ],
    notes: ["Couldn't read your review queue: HTTP 500"],
  });
  assert.deepEqual(d.sections.map((s) => s.id), ["to-review", "tickets", "ready", "notes"]);
  assert.equal(d.sections[0].items[0].text, "CI/sample-app #9 PR 9 — by bob");
  assert.equal(d.sections[0].items[0].url, null);
  assert.deepEqual(d.sections[1].items.map((i) => i.tone), ["warn", "neutral"]);
  assert.equal(d.sections[2].items[0].tone, "bad");
  assert.match(d.sections[2].items[0].text, /background analyze-issue run for PROJ-7 failed: Claude exited/);
  assert.equal(d.sections[2].items[1].text, "Conflict resolution ready to review: CI/sample-app#2 — Resolved 2 files");
  assert.equal(d.headline, "Needs you: 1 PR to review, 1 ready for you, 1 background run failed.");
  assert.equal(d.counts.ready, 1, "a failed run is not \"ready\"");
  assert.equal(d.counts.failedRuns, 1);
});

test("long lists are capped and untrusted text is flattened", () => {
  const many = Array.from({ length: MAX_ITEMS + 3 }, (_, i) => pr(i + 1, { title: `line\nbreak ${i}` }));
  const d = buildDigest({ now: NOW, reviewPrs: many });
  assert.equal(d.sections[0].items.length, MAX_ITEMS + 1);
  assert.equal(d.sections[0].items.at(-1).text, "…and 3 more");
  assert.ok(!d.sections[0].items[0].text.includes("\n"));
  assert.equal(d.sections[0].more.length, 3, "the hidden items travel with the section");
});

test("digestNotification makes one inbox item per day", () => {
  const d = buildDigest({ now: NOW, reviewPrs: [pr(9)] });
  assert.deepEqual(digestNotification(d, "2026-09-29"), {
    key: "digest:2026-09-29",
    kind: "digest",
    title: "Morning digest — Needs you: 1 PR to review.",
    body: "CI/sample-app #9 PR 9",
    url: null,
    urgent: false,
  });
  assert.match(digestNotification(buildDigest({ now: NOW }), "d").body, /Open ✨ Morning digest/);
});

test("digestNotification says a background run couldn't read its sources and points to the button", () => {
  const d = buildDigest({ now: NOW, notes: ["Couldn't read your pull requests: no sign-in (open the site in Chrome, or save a token in ⚙ Settings)."] });
  const n = digestNotification(d, "2026-09-29");
  assert.equal(n.title, "Morning digest — couldn't read some sources in the background");
  assert.match(n.body, /Open ✨ Morning digest/);
  assert.match(n.body, /sign-in/);
  assert.ok(!n.body.includes("Couldn't read your pull requests"));
});

test("digestNotification keeps the reasons sources couldn't be read next to the lines it does show", () => {
  const d = buildDigest({ now: NOW, tickets: [{ key: "PROJ-1", summary: "s" }], notes: ["Couldn't read your pull requests: HTTP 401."] });
  const n = digestNotification(d, "2026-09-29");
  assert.deepEqual(n.body.split("\n"), ["PROJ-1 s", "Couldn't read your pull requests: HTTP 401."]);
});

test("every label is one clean line: control, invisible and bidi characters go; markup stays literal text", () => {
  const nasty = "x\u001b[2Jy\u202Ez\u200Bw";
  const d = buildDigest({
    now: NOW,
    myPrs: [{ pr: pr(1, { project: nasty, repo: nasty, title: nasty, reviewers: [{ name: nasty, status: "NEEDS_WORK" }] }), conflicted: true, build: null }],
    reviewPrs: [pr(2, { project: nasty, authorSlug: nasty })],
    tickets: [{ key: nasty, summary: "<script>alert(1)</script>", status: nasty, priority: "High" }],
    prewarmed: [{ featureId: nasty, status: "awaiting-approval", scopeKey: `bitbucket:${nasty}`, summary: nasty }, { featureId: nasty, status: "failed", scopeKey: `jira:${nasty}`, error: nasty }],
    notes: [nasty],
  });
  const all = JSON.stringify(d);
  assert.doesNotMatch(all, /\\u001b|[\u202e\u200b\u0085]/);
  const tickets = d.sections.find((s) => s.id === "tickets").items[0].text;
  assert.match(tickets, /<script>alert\(1\)<\/script>/, "markup is data; the panel renders it with textContent");
});

test("long titles are clipped, INPROGRESS builds show as busy, an approved PR with a running build is not ok", () => {
  const d = buildDigest({
    now: NOW,
    myPrs: [
      { pr: pr(1, { title: "t".repeat(500) }), conflicted: false, build: { state: "INPROGRESS" } },
      { pr: pr(2, { approvals: 1 }), conflicted: false, build: { state: "INPROGRESS" } },
      { pr: pr(3, { approvals: 1 }), conflicted: false, build: { state: "SUCCESSFUL" } },
    ],
  });
  const items = d.sections[0].items;
  assert.deepEqual(items.map((i) => i.tone), ["busy", "busy", "ok"]);
  assert.ok(items[0].text.length < 160);
  assert.match(items[0].text, /build running/);
});

test("notes come last; a digest with only notes doesn't claim nothing needs you", () => {
  const withNotes = buildDigest({ now: NOW, reviewPrs: [pr(9)], notes: ["Couldn't read your Jira tickets: HTTP 500."] });
  assert.equal(withNotes.sections.at(-1).id, "notes");
  const onlyNotes = buildDigest({ now: NOW, notes: ["Couldn't read your pull requests: HTTP 500."] });
  assert.deepEqual(onlyNotes.sections.map((s) => s.id), ["notes"]);
  assert.doesNotMatch(onlyNotes.headline, /Nothing needs you/);
  assert.match(onlyNotes.headline, /couldn't be checked/);
});

test("digestNotification shows at most 3 lines and survives a non-finite clock", () => {
  const d = buildDigest({ now: NOW, reviewPrs: [pr(1), pr(2), pr(3), pr(4), pr(5)], tickets: [{ key: "PROJ-1", summary: "s" }, { key: "PROJ-2", summary: "t" }] });
  assert.equal(digestNotification(d, "2026-09-29").body.split("\n").length, 3);
  for (const bad of [NaN, Infinity, undefined]) {
    const x = buildDigest({ now: bad });
    assert.match(x.generatedAt, /^\d{4}-\d\d-\d\dT/);
  }
});
