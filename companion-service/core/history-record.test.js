const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const rec = require("./history-record.js");
const { openHistory } = require("./history-db.js");

const T0 = 1_000_000;
const job = (over = {}) => ({
  id: "11111111-1111-4111-8111-111111111111",
  featureId: "analyze-issue",
  status: "awaiting-approval",
  data: {},
  createdAt: T0,
  updatedAt: T0 + 5000,
  ...over,
});

test("outcomeFor maps terminal states, and only read-only features finish at awaiting-approval", () => {
  assert.equal(rec.outcomeFor(job({ status: "approved" })), "approved");
  assert.equal(rec.outcomeFor(job({ status: "rejected" })), null);
  assert.equal(rec.outcomeFor(job({ featureId: "resolve-conflict", status: "rejected" })), "discarded");
  assert.equal(rec.outcomeFor(job({ status: "failed" })), "failed");
  assert.equal(rec.outcomeFor(job({ status: "awaiting-approval" })), "completed");
  assert.equal(rec.outcomeFor(job({ featureId: "resolve-conflict", status: "awaiting-approval" })), null);
  assert.equal(rec.outcomeFor(job({ status: "running" })), null);
});

test("opsForJob returns null for a non-terminal state", () => {
  assert.equal(rec.opsForJob(job({ status: "running" }), T0 + 1), null);
});

test("an analysis records job, ticket, analysis and session items with links", () => {
  const ops = rec.opsForJob(
    job({
      scopeKey: "jira:PROJ-7",
      result: { summary: "Null check missing", files: [] },
      data: {
        issueKey: "PROJ-7",
        summary: "Login fails",
        analysis: { summary: "Missing null check in LoginForm" },
        claudeSession: { id: "22222222-2222-4222-8222-222222222222", cwd: "/w", permissionMode: "plan" },
      },
    }),
    T0 + 5000,
  );
  assert.deepEqual(ops.items.map((i) => `${i.kind}:${i.key}`).sort(), [
    "analysis:analysis:jira:PROJ-7",
    "job:job:11111111-1111-4111-8111-111111111111",
    "session:session:22222222-2222-4222-8222-222222222222",
    "ticket:jira:PROJ-7",
  ]);
  assert.ok(ops.edges.some((e) => e.from === "subject" && e.to === "analysis" && e.rel === "analyzed"));
  assert.ok(ops.edges.some((e) => e.from === "job" && e.to === "subject" && e.rel === "links"));
  assert.equal(ops.event.outcome, "completed");
  assert.equal(ops.event.durationMs, 5000);
  assert.equal(ops.event.scopeKey, "jira:PROJ-7");
});

test("resolve-conflict metrics carry file count and whether it was conflicted", () => {
  const ops = rec.opsForJob(
    job({
      featureId: "resolve-conflict",
      status: "approved",
      scopeKey: "bitbucket:CI/sample-app#3",
      data: { conflicted: true },
      result: { summary: "s", files: [{ path: "a" }, { path: "b" }] },
    }),
    T0 + 90000,
  );
  assert.deepEqual(ops.event.metrics, { durationMs: 90000, conflicted: true, filesChanged: 2 });
  assert.ok(ops.edges.some((e) => e.from === "job" && e.to === "subject" && e.rel === "fixes"));
});

test("expiredEvent describes a pruned job", () => {
  assert.deepEqual(rec.expiredEvent(job({ featureId: "resolve-conflict", status: "awaiting-approval", scopeKey: "jira:PROJ-7" }), T0 + 9000), {
    jobId: "11111111-1111-4111-8111-111111111111",
    featureId: "resolve-conflict",
    scopeKey: "jira:PROJ-7",
    status: "expired",
    at: T0 + 9000,
    durationMs: 9000,
    outcome: "expired",
    metrics: { durationMs: 9000, lastStatus: "awaiting-approval" },
  });
});

test("expiredEvent is null when the job's status already has an event", () => {
  assert.equal(rec.expiredEvent(job({ status: "failed" }), T0 + 9000), null);
  assert.equal(rec.expiredEvent(job({ status: "awaiting-approval" }), T0 + 9000), null);
});

test("redactSecrets masks tokens, passwords and URL credentials, and leaves ordinary text", () => {
  assert.equal(rec.redactSecrets("Authorization: Bearer abc123"), "Authorization: Bearer ***");
  assert.equal(rec.redactSecrets("Authorization: Basic dXNlcjpwdw=="), "Authorization: Basic ***");
  assert.equal(rec.redactSecrets("x?Token=abc&y=1"), "x?Token=***&y=1");
  assert.equal(rec.redactSecrets('password=hunter2 "secret=s3"'), 'password=*** "secret=***"');
  assert.equal(rec.redactSecrets("apikey=k1 api_key=k2"), "apikey=*** api_key=***");
  assert.equal(rec.redactSecrets("clone https://me:pw@host/x"), "clone https://***@host/x");
  assert.equal(rec.redactSecrets("Null check missing in LoginForm"), "Null check missing in LoginForm");
});

test("a failed job's stored excerpt carries no credentials", () => {
  const ops = rec.opsForJob(
    job({ status: "failed", error: "git clone https://me:hunter2@host/x failed (Authorization: Bearer abc123)" }),
    T0 + 1,
  );
  const excerpt = ops.items.find((i) => i.ref === "job").excerpt;
  assert.doesNotMatch(excerpt, /hunter2|abc123/);
});

test("applyOps writes the rows, and a recorder never throws or writes once frozen", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-rec-"));
  const h = openHistory(path.join(dir, "history.db"));
  t.after(() => { h.close(); fs.rmSync(dir, { recursive: true, force: true }); });
  const logs = [];
  const recorder = rec.createRecorder(h, { log: (m) => logs.push(m), now: () => T0 + 5000 });
  recorder.onTransition(job({ scopeKey: "jira:PROJ-7", data: { summary: "Login fails", analysis: "It is a null check." } }));
  const detail = h.getItem("PROJ-7");
  assert.equal(detail.item.title, "Login fails");
  assert.ok(detail.edges.some((e) => e.rel === "analyzed" && e.key === "analysis:jira:PROJ-7"));
  assert.equal(h.metrics({ days: 1, at: T0 + 5000 })[0].outcome, "completed");
  recorder.freeze();
  recorder.onTransition(job({ id: "33333333-3333-4333-8333-333333333333", status: "failed" }));
  assert.equal(h.getItem("job:33333333-3333-4333-8333-333333333333"), null);
  assert.deepEqual(logs, []);
  const broken = rec.createRecorder({ upsertItem() { throw new Error("disk full"); } }, { log: (m) => logs.push(m) });
  broken.onTransition(job());
  assert.match(logs[0], /disk full/);
});

// ---- Phase 6: ticket features ----

const WT = "/Users/me/gitviews/sample-app.worktrees/PROJ-7";
const SESSION_ID = "44444444-4444-4444-8444-444444444444";
const ticketToPrJob = (over = {}) =>
  job({
    featureId: "ticket-to-pr",
    scopeKey: "jira:PROJ-7",
    data: {
      issueKey: "PROJ-7",
      summary: "Login fails",
      repoKey: "ACME/sample-app",
      branch: "bugfix/PROJ-7-login-fails",
      base: "master",
      ticketWorktree: { dir: WT },
      fixStartedAt: T0,
      claudeSession: { id: SESSION_ID, cwd: WT, permissionMode: "plan" },
    },
    ...over,
  });

test("Start fix is recorded at awaiting-approval: the ticket links to its worktree, and the worktree to the session", () => {
  assert.equal(rec.outcomeFor(ticketToPrJob()), "completed");
  const ops = rec.opsForJob(ticketToPrJob(), T0 + 1000);
  const worktree = ops.items.find((i) => i.kind === "worktree");
  assert.equal(worktree.key, `worktree:${WT}`);
  assert.equal(worktree.repo, "ACME/sample-app");
  assert.equal(worktree.title, "bugfix/PROJ-7-login-fails");
  assert.deepEqual(worktree.data, { dir: WT, repoKey: "ACME/sample-app", branch: "bugfix/PROJ-7-login-fails", base: "master", fixStartedAt: T0 });
  assert.ok(ops.edges.some((e) => e.from === "subject" && e.to === "worktree" && e.rel === "worktree"));
  assert.ok(ops.edges.some((e) => e.from === "worktree" && e.to === "session" && e.rel === "links"));
  assert.equal(ops.event.metrics.startFixToPrMs, undefined);
});

test("Create PR records the PR from the ticket and the worktree, and the Start-fix-to-PR time", () => {
  const approved = ticketToPrJob({
    status: "approved",
    data: {
      ...ticketToPrJob().data,
      pr: { id: 12, title: "PROJ-7: fix login", url: "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12" },
    },
  });
  const ops = rec.opsForJob(approved, T0 + 90_000);
  const pr = ops.items.find((i) => i.kind === "pr");
  assert.equal(pr.key, "bitbucket:ACME/sample-app#12");
  assert.equal(pr.url, "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12");
  assert.ok(ops.edges.some((e) => e.from === "subject" && e.to === "pr" && e.rel === "links"));
  assert.ok(ops.edges.some((e) => e.from === "worktree" && e.to === "pr"));
  assert.ok(ops.edges.some((e) => e.from === "job" && e.to === "subject" && e.rel === "fixes"));
  assert.equal(ops.event.outcome, "approved");
  assert.equal(ops.event.metrics.startFixToPrMs, 90_000);
});

test("a workspace scan records only its timing event", () => {
  const ops = rec.opsForJob(job({ featureId: "ticket-workspace", scopeKey: "jira:PROJ-7" }), T0 + 700);
  assert.deepEqual(ops.items, []);
  assert.deepEqual(ops.edges, []);
  assert.equal(ops.event.outcome, "completed");
  assert.equal(ops.event.featureId, "ticket-workspace");
});

test("the recorded worktree and PR round-trip through the store", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-rec-"));
  const h = openHistory(path.join(dir, "history.db"));
  t.after(() => { h.close(); fs.rmSync(dir, { recursive: true, force: true }); });
  const recorder = rec.createRecorder(h, { now: () => T0 + 90_000 });
  recorder.onTransition(ticketToPrJob({
    status: "approved",
    data: { ...ticketToPrJob().data, pr: { id: 12, title: "T", url: "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12" } },
  }));
  const ticket = h.getItem("PROJ-7");
  assert.ok(ticket.edges.some((e) => e.rel === "worktree" && e.key === `worktree:${WT}` && e.direction === "out"));
  assert.ok(ticket.edges.some((e) => e.rel === "links" && e.key === "bitbucket:ACME/sample-app#12" && e.direction === "out"));
  const wt = h.getItem(`worktree:${WT}`);
  assert.equal(wt.item.kind, "worktree");
  assert.ok(wt.edges.some((e) => e.kind === "session" && e.key === `session:${SESSION_ID}`));
});

// ---- Phase 6 final review: the real store, awaiting-approval -> approved ----

const PR_URL = "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12";
const eventRows = (file) => {
  const { DatabaseSync } = require("node:sqlite");
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    return db.prepare("SELECT status, outcome, metrics_json FROM events ORDER BY at, status").all().map((r) => ({ ...r, metrics: JSON.parse(r.metrics_json) }));
  } finally {
    db.close();
  }
};

test("Start fix then Create PR through the real store: worktree->pr edge, startFixToPrMs, idempotent, COALESCE keeps earlier fields", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-rec-flow-"));
  const file = path.join(dir, "history.db");
  const h = openHistory(file);
  t.after(() => { h.close(); fs.rmSync(dir, { recursive: true, force: true }); });
  let now = T0 + 1000;
  const recorder = rec.createRecorder(h, { now: () => now });
  recorder.onTransition(ticketToPrJob());
  assert.equal(h.getItem(`worktree:${WT}`).item.data.fixStartedAt, T0);

  now = T0 + 90_000;
  // Create PR's job data carries no PR title/summary of its own here: the earlier rows keep theirs (COALESCE).
  const approved = ticketToPrJob({ status: "approved", data: { ...ticketToPrJob().data, pr: { id: 12, url: PR_URL } } });
  recorder.onTransition(approved);
  recorder.onTransition(approved); // a repeat records nothing new
  const wt = h.getItem(`worktree:${WT}`);
  assert.equal(wt.item.title, "bugfix/PROJ-7-login-fails");
  assert.ok(wt.edges.some((e) => e.rel === "links" && e.key === "bitbucket:ACME/sample-app#12" && e.direction === "out"), "worktree -> pr edge");
  assert.ok(wt.edges.some((e) => e.kind === "session" && e.key === `session:${SESSION_ID}`));
  assert.equal(wt.edges.filter((e) => e.key === "bitbucket:ACME/sample-app#12").length, 1);
  assert.equal(h.getItem("bitbucket:ACME/sample-app#12").item.url, PR_URL);
  const ev = eventRows(file);
  assert.deepEqual(ev.map((e) => e.status), ["awaiting-approval", "approved"]);
  assert.equal(ev.find((e) => e.status === "approved").metrics.startFixToPrMs, 90_000);
  assert.equal(ev.find((e) => e.status === "awaiting-approval").metrics.startFixToPrMs, undefined);
});

test("bad ticket-to-pr data records nothing it shouldn't: relative or missing worktree dir, non-finite start time, malformed PR url, a failed job", () => {
  const at = T0 + 5000;
  const base = ticketToPrJob().data;
  const kinds = (ops) => ops.items.map((i) => i.kind);
  assert.ok(!kinds(rec.opsForJob(ticketToPrJob({ data: { ...base, ticketWorktree: { dir: "relative/dir" } } }), at)).includes("worktree"));
  assert.ok(!kinds(rec.opsForJob(ticketToPrJob({ data: { ...base, ticketWorktree: undefined } }), at)).includes("worktree"));
  assert.ok(!kinds(rec.opsForJob(ticketToPrJob({ data: { ...base, ticketWorktree: {} } }), at)).includes("worktree"));
  const nan = rec.opsForJob(ticketToPrJob({ status: "approved", data: { ...base, fixStartedAt: NaN } }), at);
  assert.equal(nan.event.metrics.startFixToPrMs, undefined);
  assert.equal(nan.items.find((i) => i.kind === "worktree").data.fixStartedAt, undefined);
  const str = rec.opsForJob(ticketToPrJob({ status: "approved", data: { ...base, fixStartedAt: "5" } }), at);
  assert.equal(str.event.metrics.startFixToPrMs, undefined);
  for (const url of ["not a url", "https://bb.example/projects/ACME/repos/sample-app", "javascript:alert(1)", 5, undefined]) {
    const ops = rec.opsForJob(ticketToPrJob({ status: "approved", data: { ...base, pr: { id: 12, url } } }), at);
    assert.ok(!kinds(ops).includes("pr"), String(url));
  }
  assert.ok(!kinds(rec.opsForJob(ticketToPrJob({ status: "approved", data: { ...base, pr: { id: "12", url: PR_URL } } }), at)).includes("pr"));
  const failed = rec.opsForJob(ticketToPrJob({ status: "failed", error: "boom", data: { ...base, pr: { id: 12, url: PR_URL } } }), at);
  assert.equal(failed.event.outcome, "failed");
  assert.equal(failed.event.metrics.startFixToPrMs, undefined);
});
