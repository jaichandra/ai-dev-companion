// Pre-warmed jobs in the history: the watcher rides along in each event's
// metrics, an opened read-only result counts as "used", and prewarmMetrics
// gives the used fraction per watcher. Real SQLite in a temp dir.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const rec = require("./history-record.js");
const schema = require("./history-schema.js");
const { openHistory } = require("./history-db.js");
const { lookupJob } = require("./scope-key.js");

const T0 = 1_000_000;
const id = (n) => `11111111-1111-4111-8111-${String(n).padStart(12, "0")}`;
const job = (n, over = {}) => ({
  id: id(n),
  featureId: "resolve-conflict",
  status: "awaiting-approval",
  data: {},
  scopeKey: "bitbucket:CI/sample-app#12",
  startedVia: "watcher",
  watcher: "conflicts",
  createdAt: T0,
  updatedAt: T0,
  ...over,
});

test("a watcher's job carries the watcher into its event metrics; others don't", () => {
  const ops = rec.opsForJob(job(1, { status: "approved" }), T0 + 60_000);
  assert.deepEqual(ops.event.metrics, { durationMs: 60_000, watcher: "conflicts", tier: "claude", conflicted: false });
  const plain = rec.opsForJob(job(2, { status: "approved", startedVia: "extension", watcher: undefined }), T0 + 1);
  assert.equal(plain.event.metrics.watcher, undefined);
  assert.equal(rec.expiredEvent(job(3), T0 + 5).metrics.watcher, "conflicts");
});

test("usedEvent is the inbox open of a pre-warmed job, and only of one", () => {
  const analysis = job(4, { featureId: "analyze-issue", scopeKey: "jira:PROJ-7", watcher: "assignedBugs" });
  assert.deepEqual(rec.usedEvent(analysis, T0 + 10), {
    jobId: id(4),
    featureId: "analyze-issue",
    scopeKey: "jira:PROJ-7",
    status: "opened",
    at: T0 + 10,
    durationMs: 10,
    outcome: "used",
    metrics: { watcher: "assignedBugs", opened: true },
  });
  assert.equal(rec.usedEvent({ ...analysis, startedVia: "mcp" }, T0), null);
  assert.equal(rec.usedEvent(null, T0), null);
  assert.ok(schema.OUTCOMES.includes("used"));
});

test("summarizePrewarmed counts each job once and works out the used fraction", () => {
  const row = (n, outcome, watcher = "conflicts") => ({ job_id: id(n), outcome, metrics_json: JSON.stringify({ watcher }) });
  assert.deepEqual(
    schema.summarizePrewarmed([
      row(1, "approved"),
      row(2, "discarded"),
      row(3, "failed"),
      row(4, "expired"),
      row(5, "completed", "assignedBugs"),
      row(5, "used", "assignedBugs"),
      row(6, "completed", "assignedBugs"),
      { job_id: "x", outcome: "approved", metrics_json: "{}" },
      { job_id: "y", outcome: "approved", metrics_json: "not json" },
    ]),
    [
      { watcher: "assignedBugs", runs: 2, used: 1, discarded: 0, failed: 0, expired: 0, usedFraction: 0.5 },
      { watcher: "conflicts", runs: 4, used: 1, discarded: 1, failed: 1, expired: 1, usedFraction: 0.25 },
    ],
  );
});

test("prewarmMetrics reads the recorded events back out of the database", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "prewarm-"));
  try {
    let clock = T0;
    const history = openHistory(path.join(dir, "history.db"), { now: () => clock });
    const recorder = rec.createRecorder(history, { now: () => clock });
    recorder.onTransition(job(1, { status: "approved" }));
    recorder.onTransition(job(2, { status: "rejected" }));
    const analysis = job(3, { featureId: "analyze-issue", scopeKey: "jira:PROJ-7", watcher: "assignedBugs" });
    recorder.onTransition(analysis);
    history.recordEvent(rec.usedEvent(analysis, clock));
    recorder.onTransition(job(4, { status: "approved", startedVia: "extension", watcher: undefined }));
    assert.deepEqual(history.prewarmMetrics({ days: 30 }), [
      { watcher: "assignedBugs", runs: 1, used: 1, discarded: 0, failed: 0, expired: 0, usedFraction: 1 },
      { watcher: "conflicts", runs: 2, used: 1, discarded: 1, failed: 0, expired: 0, usedFraction: 0.5 },
    ]);
    clock += 31 * 24 * 60 * 60 * 1000;
    assert.deepEqual(history.prewarmMetrics({ days: 30 }), []);
    history.close();
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a page finds a watcher's job by its scopeKey, like an MCP-started one", () => {
  const found = lookupJob([job(1), job(2, { startedVia: "extension" })], "bitbucket:CI/sample-app#12", "resolve-conflict");
  assert.equal(found.id, id(1));
});
