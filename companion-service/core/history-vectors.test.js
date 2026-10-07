const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { openHistory } = require("./history-db.js");
const schema = require("./history-schema.js");

function open(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-vec-"));
  const file = path.join(dir, "history.db");
  const h = openHistory(file);
  t.after(() => {
    try { h.close(); } catch { /* already closed */ }
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return { h, file, dir };
}

const MODEL = "Qwen3-Embedding-8B";
const DAY = 24 * 60 * 60 * 1000;

test("an existing v1 database is migrated to v2 and keeps its rows", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "history-vec-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const file = path.join(dir, "history.db");
  const { DatabaseSync } = require("node:sqlite");
  const raw = new DatabaseSync(file);
  raw.exec(schema.MIGRATIONS[0]);
  raw.exec("PRAGMA user_version = 1");
  raw.prepare("INSERT INTO items (kind, key, title, updated_at) VALUES ('ticket', 'jira:PROJ-1', 'Old ticket', 1)").run();
  raw.close();
  const h = openHistory(file);
  t.after(() => h.close());
  assert.equal(h.getItem("PROJ-1").item.title, "Old ticket");
  assert.deepEqual(h.vectorStats(), { vectors: 0, embeddable: 1, models: [] });
  const check = new DatabaseSync(file);
  assert.equal(check.prepare("PRAGMA user_version").get().user_version, schema.SCHEMA_VERSION);
  assert.equal(schema.SCHEMA_VERSION, 2);
  check.close();
});

test("pendingEmbeddings lists items with text and no vector (or a stale one), newest first", (t) => {
  const { h } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login fails", at: 10 });
  const b = h.upsertItem({ kind: "analysis", key: "analysis:jira:PROJ-1", title: "Analysis of PROJ-1", excerpt: "Null check", at: 20 });
  h.upsertItem({ kind: "job", key: "job:1", title: "a job", at: 30 });
  h.upsertItem({ kind: "pr", key: "CI/sample-app#3", at: 40 });
  assert.deepEqual(h.pendingEmbeddings({ model: MODEL }).map((i) => i.id), [b, a], "jobs and text-less items are skipped");
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 10 });
  assert.deepEqual(h.pendingEmbeddings({ model: MODEL }).map((i) => i.id), [b]);
  assert.deepEqual(h.pendingEmbeddings({ model: "other-model" }).map((i) => i.id), [b, a], "another model re-embeds");
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login fails on SSO", at: 50 });
  assert.deepEqual(h.pendingEmbeddings({ model: MODEL }).map((i) => i.id), [a, b], "an updated item is pending again");
  assert.deepEqual(h.pendingEmbeddings({ model: MODEL, limit: 1 }).map((i) => i.id), [a]);
  assert.deepEqual(h.pendingEmbeddings({ model: "" }), []);
});

test("nearest ranks by cosine within one model and size, above the threshold", (t) => {
  const { h } = open(t);
  const ids = ["PROJ-1", "PROJ-2", "PROJ-3", "PROJ-4"].map((k, i) => h.upsertItem({ kind: "ticket", key: k, title: k, at: i + 1 }));
  h.saveVector({ itemId: ids[0], model: MODEL, vector: [1, 0, 0], itemUpdatedAt: 1 });
  h.saveVector({ itemId: ids[1], model: MODEL, vector: [0.9, 0.1, 0], itemUpdatedAt: 2 });
  h.saveVector({ itemId: ids[2], model: MODEL, vector: [0, 1, 0], itemUpdatedAt: 3 });
  h.saveVector({ itemId: ids[3], model: "other", vector: [1, 0, 0], itemUpdatedAt: 4 });
  const q = h.vectorFor(ids[0], MODEL);
  assert.ok(q instanceof Float32Array && q.length === 3);
  const hits = h.nearest({ model: MODEL, vector: q });
  assert.deepEqual(hits.map((x) => x.key), ["jira:PROJ-1", "jira:PROJ-2"], "PROJ-3 is below the threshold, PROJ-4 another model");
  assert.equal(hits[0].score, 1);
  assert.equal(h.nearest({ model: MODEL, vector: new Float32Array([1, 0]) }).length, 0, "another size never matches");
  assert.equal(h.nearest({ model: MODEL, vector: q, kinds: ["analysis"] }).length, 0);
  assert.equal(h.vectorFor(ids[0], "other"), null);
  assert.deepEqual(h.vectorStats().models, [
    { model: MODEL, dim: 3, count: 3 },
    { model: "other", dim: 3, count: 1 },
  ]);
  assert.throws(() => h.saveVector({ itemId: ids[0], model: MODEL, vector: [0, 0, 0], itemUpdatedAt: 1 }), /zeros/);
});

test("searchAny matches any word, only in the similar-item kinds", (t) => {
  const { h } = open(t);
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login page fails for SSO users" });
  h.upsertItem({ kind: "analysis", key: "analysis:jira:PROJ-2", title: "Analysis of PROJ-2", excerpt: "The SSO token refresh races" });
  h.upsertItem({ kind: "job", key: "job:1", title: "SSO job" });
  const keys = h.searchAny("Users can't sign in with SSO after the upgrade").map((i) => i.key).sort();
  assert.deepEqual(keys, ["analysis:jira:PROJ-2", "jira:PROJ-1"]);
  assert.deepEqual(h.searchAny("the and of"), []);
  assert.deepEqual(h.searchAny('") OR 1=1 --').map((i) => i.key), []);
});

test("forget and retention delete an item's vector with it", (t) => {
  const { h } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one", at: 1 });
  const b = h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "two", at: 40 * DAY });
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  h.saveVector({ itemId: b, model: MODEL, vector: [0, 1], itemUpdatedAt: 2 });
  assert.equal(h.vectorStats().vectors, 2);
  h.prune(30, 50 * DAY);
  assert.equal(h.vectorStats().vectors, 1, "the pruned item's vector is gone");
  assert.equal(h.vectorFor(a, MODEL), null);
  h.forget("PROJ-2");
  assert.equal(h.vectorStats().vectors, 0, "the forgotten item's vector is gone");
});

test("an analysis report's tldr becomes the analysis item's excerpt (what gets embedded)", () => {
  const { opsForJob } = require("./history-record.js");
  const ops = opsForJob(
    {
      id: "j1",
      featureId: "analyze-issue",
      status: "awaiting-approval",
      scopeKey: "jira:PROJ-7",
      createdAt: 0,
      data: { summary: "Login fails", analysis: { tldr: "Null check missing in LoginForm", nextSteps: [] } },
    },
    1000,
  );
  assert.equal(ops.items.find((i) => i.kind === "analysis").excerpt, "Null check missing in LoginForm");
});

test("nearest ignores a vector older than its item, and saveVector validates its ids", (t) => {
  const { h } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one", at: 1 });
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  const q = h.vectorFor(a, MODEL);
  assert.equal(h.nearest({ model: MODEL, vector: q }).length, 1);
  h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one, rewritten", at: 5 });
  assert.equal(h.nearest({ model: MODEL, vector: q }).length, 0, "a stale vector must not rank");
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 5 });
  assert.equal(h.nearest({ model: MODEL, vector: q }).length, 1);
  for (const bad of [{ itemId: "x", itemUpdatedAt: 1 }, { itemId: 0, itemUpdatedAt: 1 }, { itemId: a, itemUpdatedAt: NaN }, { itemId: a }]) {
    assert.throws(() => h.saveVector({ model: MODEL, vector: [1, 0], ...bad }), /Invalid/, JSON.stringify(bad));
  }
});

test("a corrupt stored blob is skipped, not thrown on", (t) => {
  const { h, file } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one", at: 1 });
  const b = h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "two", at: 1 });
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  h.saveVector({ itemId: b, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  const { DatabaseSync } = require("node:sqlite");
  const raw = new DatabaseSync(file);
  raw.prepare("UPDATE item_vectors SET vec = ? WHERE item_id = ?").run(new Uint8Array([1, 2, 3]), a);
  raw.close();
  assert.equal(h.vectorFor(a, MODEL), null);
  assert.deepEqual(h.nearest({ model: MODEL, vector: new Float32Array([1, 0]) }).map((x) => x.key), ["jira:PROJ-2"]);
});

test("reopening a v2 database keeps its vectors; a newer database is refused", (t) => {
  const { h, file } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one", at: 1 });
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  h.close();
  const again = openHistory(file);
  assert.equal(again.vectorStats().vectors, 1);
  assert.ok(again.vectorFor(a, MODEL));
  again.close();
  const { DatabaseSync } = require("node:sqlite");
  const raw = new DatabaseSync(file);
  raw.exec(`PRAGMA user_version = ${schema.SCHEMA_VERSION + 1}`);
  raw.close();
  assert.throws(() => openHistory(file), /newer companion/);
});

test("placeholder vectors (one number) don't count as real vectors and never match", (t) => {
  const { h } = open(t);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one", at: 1 });
  h.saveVector({ itemId: a, model: MODEL, vector: [1], itemUpdatedAt: 1 });
  assert.equal(h.hasVectors(MODEL), false);
  assert.equal(h.nearest({ model: MODEL, vector: new Float32Array([1, 0]) }).length, 0);
  const b = h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "two", at: 1 });
  h.saveVector({ itemId: b, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  assert.equal(h.hasVectors(MODEL), true);
  assert.equal(h.hasVectors("other"), false);
});
