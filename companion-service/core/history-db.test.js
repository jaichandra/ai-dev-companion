const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { openHistory } = require("./history-db.js");

function open(t, now) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-db-"));
  const file = path.join(dir, "history.db");
  const h = openHistory(file, now ? { now } : {});
  t.after(() => {
    try { h.close(); } catch { /* already closed */ }
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return { h, file, dir };
}

test("opens the database (and its WAL files) at mode 0600", { skip: process.platform === "win32" }, (t) => {
  const { h, file, dir } = open(t);
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "x" });
  for (const name of fs.readdirSync(dir).filter((n) => n.startsWith("history.db"))) {
    assert.equal(fs.statSync(path.join(dir, name)).mode & 0o777, 0o600, name);
  }
  assert.ok(fs.existsSync(file));
});

test("upsertItem keeps the same id and earlier fields when a later call omits them", (t) => {
  const { h } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "First title", repo: "CI/sample-app" });
  const b = h.upsertItem({ kind: "ticket", key: "proj-1", excerpt: "later excerpt" });
  assert.equal(a, b);
  const got = h.getItem("PROJ-1");
  assert.equal(got.item.title, "First title");
  assert.equal(got.item.repo, "CI/sample-app");
  assert.equal(got.item.excerpt, "later excerpt");
  assert.equal(got.item.key, "jira:PROJ-1");
});

test("getItem returns edges in both directions and facts with parsed values", (t) => {
  const { h } = open(t);
  const ticket = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login fails" });
  const analysis = h.upsertItem({ kind: "analysis", key: "analysis:jira:PROJ-1", title: "Analysis" });
  const job = h.upsertItem({ kind: "job", key: "job:abc", title: "analyze" });
  h.addEdge({ src: ticket, dst: analysis, rel: "analyzed" });
  h.addEdge({ src: job, dst: ticket, rel: "links" });
  h.addFact({ itemId: ticket, kind: "note", value: { severity: "high" }, provenance: "user" });
  const got = h.getItem("PROJ-1");
  assert.deepEqual(
    got.edges.map((e) => `${e.direction}:${e.rel}:${e.key}`).sort(),
    ["in:links:job:abc", "out:analyzed:analysis:jira:PROJ-1"],
  );
  assert.deepEqual(got.facts.map((f) => [f.kind, f.value, f.provenance]), [["note", { severity: "high" }, "user"]]);
  assert.equal(h.getItem("PROJ-999"), null);
});

test("search finds items by title and excerpt words and survives hostile input", (t) => {
  const { h } = open(t);
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login page times out", excerpt: "flaky testcafe" });
  h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "Billing export" });
  assert.deepEqual(h.search("login").map((r) => r.key), ["jira:PROJ-1"]);
  assert.deepEqual(h.search("testcafe").map((r) => r.key), ["jira:PROJ-1"]);
  assert.deepEqual(h.search('"; DROP TABLE items; --'), []);
  assert.deepEqual(h.search("   "), []);
  assert.equal(h.getItem("PROJ-2").item.title, "Billing export");
});

test("forget removes the item, its edges, facts, search hit and its events", (t) => {
  const { h } = open(t);
  const ticket = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login" });
  const job = h.upsertItem({ kind: "job", key: "job:j1", title: "job" });
  h.addEdge({ src: job, dst: ticket, rel: "links" });
  h.addFact({ itemId: ticket, kind: "note", value: 1, provenance: "user" });
  h.recordEvent({ jobId: "j1", featureId: "analyze-issue", scopeKey: "jira:PROJ-1", status: "awaiting-approval", outcome: "completed" });
  assert.deepEqual(h.forget("PROJ-1"), { items: 1, events: 1 });
  assert.equal(h.getItem("PROJ-1"), null);
  assert.deepEqual(h.search("login"), []);
  assert.deepEqual(h.getItem("job:j1").edges, []);
  assert.deepEqual(h.forget("PROJ-1"), { items: 0, events: 0 });
});

test("prune drops only items and events older than the retention window", (t) => {
  const day = 86400000;
  const { h } = open(t);
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "old", at: 1000 });
  h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "new", at: 200 * day });
  h.recordEvent({ jobId: "a", featureId: "f", status: "failed", outcome: "failed", at: 1000 });
  h.recordEvent({ jobId: "b", featureId: "f", status: "failed", outcome: "failed", at: 200 * day });
  assert.deepEqual(h.prune(180, 201 * day), { items: 1, events: 1 });
  assert.equal(h.getItem("PROJ-1"), null);
  assert.ok(h.getItem("PROJ-2"));
});

test("metrics summarises events inside the window and recordEvent is idempotent per job+status", (t) => {
  const day = 86400000;
  const { h } = open(t);
  h.recordEvent({ jobId: "a", featureId: "resolve-conflict", status: "approved", outcome: "approved", at: 40 * day, durationMs: 100 });
  h.recordEvent({ jobId: "a", featureId: "resolve-conflict", status: "approved", outcome: "approved", at: 40 * day, durationMs: 300 });
  h.recordEvent({ jobId: "b", featureId: "resolve-conflict", status: "approved", outcome: "approved", at: 1, durationMs: 999 });
  assert.deepEqual(h.metrics({ days: 30, at: 41 * day }), [
    { featureId: "resolve-conflict", outcome: "approved", count: 1, medianMs: 300, avgMs: 300 },
  ]);
});

test("rejects unknown kinds, relations, provenance and outcomes", (t) => {
  const { h } = open(t);
  assert.throws(() => h.upsertItem({ kind: "nope", key: "x" }), /kind/);
  assert.throws(() => h.upsertItem({ kind: "ticket", key: "" }), /key/);
  const id = h.upsertItem({ kind: "ticket", key: "PROJ-1" });
  assert.throws(() => h.addEdge({ src: id, dst: id, rel: "nope" }), /relation/);
  assert.throws(() => h.addFact({ itemId: id, kind: "n", value: 1, provenance: "nope" }), /provenance/);
  assert.throws(() => h.recordEvent({ jobId: "a", featureId: "f", status: "s", outcome: "nope" }), /outcome/);
});

test("refuses a database written by a newer schema", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-db-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const file = path.join(dir, "history.db");
  const h = openHistory(file);
  h.close();
  const { DatabaseSync } = require("node:sqlite");
  const raw = new DatabaseSync(file);
  raw.exec("PRAGMA user_version = 99");
  raw.close();
  assert.throws(() => openHistory(file), /newer/);
});

test("tightens an existing, looser database file to 0600", { skip: process.platform === "win32" }, (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-db-"));
  const file = path.join(dir, "history.db");
  fs.writeFileSync(file, "", { mode: 0o644 });
  fs.chmodSync(file, 0o644);
  const h = openHistory(file);
  t.after(() => { h.close(); fs.rmSync(dir, { recursive: true, force: true }); });
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "t", at: 1 });
  assert.equal(h.getItem("PROJ-1").item.key, "jira:PROJ-1");
});
