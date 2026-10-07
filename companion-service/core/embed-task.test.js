const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { openHistory } = require("./history-db.js");
const { createEmbedTask } = require("./embed-task.js");
const similar = require("./similar.js");

const MODEL = "Qwen3-Embedding-8B";

function open(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "embed-task-"));
  const h = openHistory(path.join(dir, "history.db"));
  t.after(() => {
    h.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return h;
}

function fakeLlm({ state = "ready", fail = false } = {}) {
  const calls = [];
  return {
    calls,
    detect: async () => ({ state }),
    settings: () => ({ embeddingModel: MODEL }),
    embed: async (texts) => {
      calls.push(texts);
      if (fail) throw new Error("The LLM proxy answered HTTP 500. Bearer sk-secret");
      return texts.map((t, i) => [t.length, i + 1, 1]);
    },
  };
}

test("with no proxy the task records 'skipped: no proxy' and sends nothing", async (t) => {
  const history = open(t);
  history.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login fails" });
  const llm = fakeLlm({ state: "no-key" });
  const r = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.deepEqual(r, { skipped: "no proxy", proxy: "no-key", embedded: 0 });
  assert.equal(llm.calls.length, 0);
});

test("turned off, or without a history, it does nothing", async (t) => {
  const history = open(t);
  const llm = fakeLlm();
  assert.equal((await createEmbedTask({ history, llm, getConfig: () => ({ similar: { enabled: false } }) })()).skipped, "similar items are off");
  assert.equal((await createEmbedTask({ history: undefined, llm, getConfig: () => ({}) })()).skipped, "local history is off");
  assert.equal(llm.calls.length, 0);
});

test("embeds the pending items in batches, redacted and clipped, and stores their vectors", async (t) => {
  const history = open(t);
  for (let i = 1; i <= 20; i++) history.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Ticket ${i} token=abc123`, at: i });
  history.upsertItem({ kind: "analysis", key: "analysis:jira:PROJ-1", title: "Analysis of PROJ-1", excerpt: "x".repeat(9000), at: 30 });
  history.upsertItem({ kind: "job", key: "job:1", title: "not embedded", at: 40 });
  const llm = fakeLlm();
  const r = await createEmbedTask({ history, llm, getConfig: () => ({}), now: () => 99 })();
  assert.deepEqual(r, { embedded: 21, model: MODEL, tier: "onprem", more: false });
  assert.deepEqual(llm.calls.map((c) => c.length), [16, 5]);
  const sent = llm.calls.flat();
  assert.ok(sent.every((text) => text.length <= similar.EMBED_TEXT_MAX));
  assert.ok(sent.some((text) => text.includes("token=***")) && !sent.some((text) => text.includes("abc123")), "secrets are masked");
  assert.equal(history.vectorStats().vectors, 21);
  assert.deepEqual(history.pendingEmbeddings({ model: MODEL }), []);
  const again = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.deepEqual(again, { embedded: 0, model: MODEL, tier: "onprem" }, "nothing new, no call");
  assert.equal(llm.calls.length, 2);
});

test("at most 48 items a run; a failed call reports what it did so far", async (t) => {
  const history = open(t);
  for (let i = 1; i <= 60; i++) history.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Ticket ${i}`, at: i });
  const first = await createEmbedTask({ history, llm: fakeLlm(), getConfig: () => ({}) })();
  assert.equal(first.embedded, 48);
  assert.equal(first.more, true);
  const failed = await createEmbedTask({ history, llm: fakeLlm({ fail: true }), getConfig: () => ({}) })();
  assert.equal(failed.outcome, "failed");
  assert.equal(failed.embedded, 0);
  assert.match(failed.error, /HTTP 500/);
});

test("exactly 48 pending is not 'more'; 49 is", async (t) => {
  const history = open(t);
  for (let i = 1; i <= 48; i++) history.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Ticket ${i}`, at: i });
  assert.equal((await createEmbedTask({ history, llm: fakeLlm(), getConfig: () => ({}) })()).more, false);
  for (let i = 101; i <= 149; i++) history.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Ticket ${i}`, at: i });
  const r = await createEmbedTask({ history, llm: fakeLlm(), getConfig: () => ({}) })();
  assert.equal(r.embedded, 48);
  assert.equal(r.more, true);
});

test("a failed second batch keeps the first batch's vectors", async (t) => {
  const history = open(t);
  for (let i = 1; i <= 20; i++) history.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Ticket ${i}`, at: i });
  const llm = fakeLlm();
  let n = 0;
  const embed = llm.embed;
  llm.embed = async (texts) => {
    if (++n === 2) throw new Error("boom");
    return embed(texts);
  };
  const r = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.equal(r.outcome, "failed");
  assert.equal(r.embedded, 16);
  assert.equal(history.vectorStats().vectors, 16);
  assert.equal(history.pendingEmbeddings({ model: MODEL }).length, 4);
});

test("a model the proxy refuses fails the run and stores nothing", async (t) => {
  const history = open(t);
  history.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login fails" });
  const llm = fakeLlm();
  llm.embed = async () => {
    throw new Error('The model "evil-model" is not in llmProxy.allowedModels.');
  };
  const r = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.equal(r.outcome, "failed");
  assert.match(r.error, /allowedModels/);
  assert.equal(history.vectorStats().vectors, 0);
});

test("one unsavable vector does not wedge the queue: it is skipped and the rest are stored", async (t) => {
  const history = open(t);
  for (let i = 1; i <= 5; i++) history.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Ticket ${i}`, at: i });
  const llm = fakeLlm();
  llm.embed = async (texts) => texts.map((_, i) => (i === 0 ? [0, 0, 0] : i === 1 ? [NaN, 1] : [1, 2, 3]));
  const r = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.equal(r.embedded, 3);
  assert.equal(r.skippedItems, 2);
  assert.equal(r.outcome, undefined);
  assert.deepEqual(history.pendingEmbeddings({ model: MODEL }), [], "the bad ones are not first in line next time");
  const again = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.equal(again.embedded, 0);
  assert.equal(llm.calls.length, 0);
});

test("items with nothing to embed are marked done so they cannot starve the others", async (t) => {
  const history = open(t);
  history.upsertItem({ kind: "ticket", key: "PROJ-1", title: String.fromCodePoint(0x200b, 0x200b), at: 5 });
  history.upsertItem({ kind: "ticket", key: "PROJ-2", title: "Real ticket", at: 1 });
  const llm = fakeLlm();
  const r = await createEmbedTask({ history, llm, getConfig: () => ({}) })();
  assert.equal(r.embedded, 1);
  assert.equal(r.skippedItems, 1);
  assert.deepEqual(history.pendingEmbeddings({ model: MODEL }), []);
  assert.equal(llm.calls.flat().length, 1);
});
