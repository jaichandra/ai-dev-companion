const test = require("node:test");
const assert = require("node:assert/strict");
const fmt = require("./cli-format.js");

const NOW = 10_000_000;
const jobs = [
  { id: "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa", featureId: "resolve-conflict", status: "awaiting-approval", scopeKey: "bitbucket:CI/sample-app#12", updatedAt: NOW - 5 * 60 * 1000, summary: "Merged master in" },
  { id: "22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb", featureId: "analyze-issue", status: "failed", scopeKey: "jira:PROJ-7", updatedAt: NOW - 3 * 3600 * 1000, error: "Jira said no" },
];

test("formatJobs prints one line per job with a short id, age and the summary or error", () => {
  const out = fmt.formatJobs(jobs, NOW);
  const lines = out.split("\n");
  assert.equal(lines.length, 2);
  assert.match(lines[0], /^11111111 +resolve-conflict +awaiting-approval +5m ago +bitbucket:CI\/sample-app#12 +Merged master in$/);
  assert.match(lines[1], /^22222222 +analyze-issue +failed +3h ago +jira:PROJ-7 +Jira said no$/);
  assert.equal(fmt.formatJobs([], NOW), "No jobs.");
});

test("formatStatus counts jobs by status", () => {
  const out = fmt.formatStatus(jobs, NOW, "http://127.0.0.1:8787");
  assert.match(out, /running at http:\/\/127\.0\.0\.1:8787/);
  assert.match(out, /2 jobs/);
  assert.match(out, /1 awaiting-approval/);
  assert.match(out, /1 failed/);
});

test("formatHistory shows the item, its links in both directions and its facts", () => {
  const out = fmt.formatHistory({
    item: { kind: "ticket", key: "jira:PROJ-7", title: "Login fails", repo: "CI/sample-app", updatedAt: NOW - 2 * 86400000, excerpt: "It times out" },
    edges: [
      { direction: "out", rel: "analyzed", key: "analysis:jira:PROJ-7", title: "Analysis of PROJ-7" },
      { direction: "in", rel: "links", key: "job:1111", title: "analyze-issue awaiting-approval" },
    ],
    facts: [{ kind: "note", value: { severity: "high" }, provenance: "user" }],
  }, NOW);
  assert.match(out, /jira:PROJ-7 \(ticket\)/);
  assert.match(out, /Login fails/);
  assert.match(out, /2d ago/);
  assert.match(out, /→ analyzed +analysis:jira:PROJ-7/);
  assert.match(out, /← links +job:1111/);
  assert.match(out, /note \(user\): {"severity":"high"}/);
});

test("formatMetrics prints a table of counts and humanized durations", () => {
  const out = fmt.formatMetrics({
    days: 30,
    metrics: [{ featureId: "resolve-conflict", outcome: "approved", count: 3, medianMs: 125000, avgMs: 3600000 }],
    matches: [{ kind: "ticket", key: "jira:PROJ-7", title: "Login fails" }],
  });
  assert.match(out, /last 30 days/);
  assert.match(out, /resolve-conflict +approved +3 +2m 5s +1h 0m/);
  assert.match(out, /jira:PROJ-7 +Login fails/);
});

test("oneLine drops control, C1, zero-width and bidi characters; every formatter uses it", () => {
  assert.equal(fmt.oneLine("a\x1b[2Jb‮c​d\u0085e\x7f f\n g"), "a[2Jbcde f g");
  const nasty = "x\x1b[2Jy‮";
  const out = [
    fmt.formatJobs([{ id: nasty, featureId: nasty, status: nasty, scopeKey: nasty, updatedAt: NOW, summary: nasty }], NOW),
    fmt.formatStatus([{ status: nasty }], NOW, nasty),
    fmt.formatHistory({ item: { key: nasty, kind: nasty, title: nasty, repo: nasty, updatedAt: NOW, excerpt: nasty }, edges: [{ direction: "out", rel: nasty, key: nasty, title: nasty }], facts: [{ kind: nasty, provenance: nasty, value: nasty }] }, NOW),
    fmt.formatMetrics({ days: nasty, metrics: [{ featureId: nasty, outcome: nasty, count: nasty, medianMs: 1, avgMs: 1 }], prewarmed: [{ watcher: nasty, runs: nasty, used: nasty, usedFraction: 0.5 }], matches: [{ key: nasty, title: nasty }] }),
    fmt.formatInbox({ unseen: nasty, items: [{ title: nasty, body: nasty, url: nasty, createdAt: NOW }] }, NOW),
    fmt.formatDigest({ headline: nasty, sections: [{ title: nasty, items: [{ text: nasty }] }] }),
    fmt.formatRisk({ skipped: true, reason: nasty }),
    fmt.formatRisk({ summary: nasty, lines: [{ text: nasty }] }),
  ].join("\n");
  assert.doesNotMatch(out, /[\u0000-\u0009\u000b-\u001f\u007f-\u009f‮]/);
  assert.equal(fmt.formatRisk(null), "companion precheck: skipped — no usable answer");
});
