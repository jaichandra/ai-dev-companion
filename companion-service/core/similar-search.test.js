const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { openHistory } = require("./history-db.js");
const { createSimilarSearch } = require("./similar-search.js");

const MODEL = "Qwen3-Embedding-8B";

function open(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "similar-search-"));
  const h = openHistory(path.join(dir, "history.db"));
  t.after(() => {
    h.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return h;
}

/** Three past tickets: two about SSO login, one about billing. */
function seed(h) {
  const t1 = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "Login fails for SSO users", at: 10 });
  h.upsertItem({ kind: "analysis", key: "analysis:jira:PROJ-1", repo: "ACME/sample-app", title: "Analysis of PROJ-1", excerpt: "The SSO token refresh races the redirect", at: 11 });
  const t2 = h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "Sign-in loop after the session expires", at: 20 });
  const t3 = h.upsertItem({ kind: "ticket", key: "PROJ-3", title: "Billing export has the wrong totals", at: 30 });
  h.upsertItem({ kind: "job", key: "job:x", title: "SSO login job", at: 40 });
  return { t1, t2, t3 };
}

function fakeLlm({ ready = true, vector = [1, 0, 0], fail = false } = {}) {
  const calls = [];
  return {
    calls,
    settings: () => ({ embeddingModel: MODEL }),
    available: async () => ready,
    embed: async (texts) => {
      calls.push(texts);
      if (fail) throw new Error("proxy down");
      return texts.map(() => vector);
    },
  };
}

test("without the proxy it ranks by shared words, groups a ticket with its analysis and leaves out jobs", async (t) => {
  const h = open(t);
  seed(h);
  const search = createSimilarSearch({ history: h, llm: fakeLlm({ ready: false }), getConfig: () => ({}) });
  const r = await search.find({ text: "SSO users see a login error" });
  assert.equal(r.mode, "text");
  assert.deepEqual(r.items.map((i) => i.key), ["jira:PROJ-1"]);
  assert.deepEqual(r.items[0], {
    key: "jira:PROJ-1",
    kind: "ticket",
    title: "Login fails for SSO users",
    analysis: "The SSO token refresh races the redirect",
    repo: "ACME/sample-app",
    updatedAt: 11,
    score: r.items[0].score,
    via: ["text"],
  });
});

test("with vectors it also finds tickets that share no words, and leaves the ticket itself out", async (t) => {
  const h = open(t);
  const { t1, t2, t3 } = seed(h);
  h.saveVector({ itemId: t1, model: MODEL, vector: [1, 0, 0], itemUpdatedAt: 10 });
  h.saveVector({ itemId: t2, model: MODEL, vector: [0.9, 0.2, 0], itemUpdatedAt: 20 });
  h.saveVector({ itemId: t3, model: MODEL, vector: [0, 0, 1], itemUpdatedAt: 30 });
  const llm = fakeLlm();
  const search = createSimilarSearch({ history: h, llm, getConfig: () => ({}) });
  const r = await search.find({ key: "PROJ-1" });
  assert.equal(r.mode, "vector+text");
  assert.deepEqual(r.items.map((i) => i.key), ["jira:PROJ-2"], "PROJ-1 itself is excluded; PROJ-3 is too far");
  assert.deepEqual(r.items[0].via, ["vector"]);
  assert.equal(llm.calls.length, 0, "the stored vector is the query; nothing is sent");
  const fresh = await search.find({ text: "Users bounce back to the sign-in page" });
  assert.equal(llm.calls.length, 1, "new text is embedded once");
  assert.deepEqual(fresh.items.map((i) => i.key), ["jira:PROJ-1", "jira:PROJ-2"]);
});

test("a failing proxy falls back to words, and k caps the list", async (t) => {
  const h = open(t);
  const { t1 } = seed(h);
  h.saveVector({ itemId: t1, model: MODEL, vector: [1, 0, 0], itemUpdatedAt: 10 });
  const llm = fakeLlm({ fail: true });
  const search = createSimilarSearch({ history: h, llm, getConfig: () => ({}) });
  const r = await search.find({ text: "SSO login session billing totals", k: 2 });
  assert.equal(llm.calls.length, 1, "the proxy was tried");
  assert.equal(r.mode, "text");
  assert.equal(r.items.length, 2);
});

test("off, no history, or nothing to compare", async (t) => {
  const h = open(t);
  seed(h);
  const off = createSimilarSearch({ history: h, llm: fakeLlm(), getConfig: () => ({ similar: { enabled: false } }) });
  assert.deepEqual(await off.find({ key: "PROJ-1" }), { enabled: false, mode: "off", items: [] });
  await assert.rejects(createSimilarSearch({ history: undefined, getConfig: () => ({}) }).find({ text: "x" }), /history is off/);
  const search = createSimilarSearch({ history: h, llm: fakeLlm({ ready: false }), getConfig: () => ({}) });
  await assert.rejects(search.find({ key: "PROJ-99" }), /Nothing is recorded/);
  await assert.rejects(search.find({}), /Give a key or some text/);
  assert.equal((await search.find({ key: "PROJ-99", text: "billing totals wrong" })).items[0].key, "jira:PROJ-3", "text works for a ticket not yet recorded");
});

test("with no vectors stored nothing is sent to the proxy and the mode is 'text'", async (t) => {
  const h = open(t);
  seed(h);
  const llm = fakeLlm();
  const r = await createSimilarSearch({ history: h, llm, getConfig: () => ({}) }).find({ text: "SSO login" });
  assert.equal(r.mode, "text");
  assert.equal(llm.calls.length, 0);
  const t3 = h.getItem("PROJ-3").item.id;
  h.saveVector({ itemId: t3, model: MODEL, vector: [1], itemUpdatedAt: 30 });
  await createSimilarSearch({ history: h, llm, getConfig: () => ({}) }).find({ text: "SSO login" });
  assert.equal(llm.calls.length, 0, "a placeholder vector isn't a real one");
});

test("a query is masked before it is embedded, and an unavailable proxy is not called", async (t) => {
  const h = open(t);
  const { t1 } = seed(h);
  h.saveVector({ itemId: t1, model: MODEL, vector: [1, 0, 0], itemUpdatedAt: 10 });
  const llm = fakeLlm();
  await createSimilarSearch({ history: h, llm, getConfig: () => ({}) }).find({ text: "SSO login token=abc123secret" + String.fromCodePoint(7) + " fails" });
  assert.equal(llm.calls.length, 1);
  assert.ok(!llm.calls[0][0].includes("abc123secret"), llm.calls[0][0]);
  assert.ok(!llm.calls[0][0].includes(String.fromCodePoint(7)));
  const down = fakeLlm({ ready: false });
  const r = await createSimilarSearch({ history: h, llm: down, getConfig: () => ({}) }).find({ text: "SSO login fails" });
  assert.equal(down.calls.length, 0);
  assert.equal(r.mode, "text");
});

test("a slow embedding times out, and a missing or broken llm or store leaves the words", async (t) => {
  const h = open(t);
  const { t1 } = seed(h);
  h.saveVector({ itemId: t1, model: MODEL, vector: [1, 0, 0], itemUpdatedAt: 10 });
  const slow = { ...fakeLlm(), embed: () => new Promise(() => {}) };
  const started = Date.now();
  const r = await createSimilarSearch({ history: h, llm: slow, getConfig: () => ({}), embedTimeoutMs: 30 }).find({ text: "SSO login fails" });
  assert.equal(r.mode, "text");
  assert.ok(Date.now() - started < 2000);
  assert.equal((await createSimilarSearch({ history: h, llm: undefined, getConfig: () => ({}) }).find({ text: "SSO login" })).mode, "text");
  const noSettings = { ...fakeLlm(), settings: () => { throw new Error("config broke"); } };
  assert.equal((await createSimilarSearch({ history: h, llm: noSettings, getConfig: () => ({}) }).find({ text: "SSO login" })).mode, "text");
  const brokenStore = Object.create(h, { vectorFor: { value: () => { throw new Error("db"); } } });
  const viaKey = await createSimilarSearch({ history: brokenStore, llm: fakeLlm(), getConfig: () => ({}) }).find({ key: "PROJ-1" });
  assert.equal(viaKey.mode, "text");
});

test("a PR result is hydrated as a pr, and a large k is capped at 20", async (t) => {
  const h = open(t);
  h.upsertItem({ kind: "pr", key: "bitbucket:CI/sample-app#7", title: "Fix the login redirect", at: 5 });
  for (let i = 1; i <= 25; i++) h.upsertItem({ kind: "ticket", key: `PROJ-${i}`, title: `Login redirect problem ${i}`, at: i });
  const search = createSimilarSearch({ history: h, llm: fakeLlm({ ready: false }), getConfig: () => ({}) });
  const r = await search.find({ text: "login redirect", k: 100 });
  assert.equal(r.items.length, 20);
  const pr = (await search.find({ text: "Fix the login redirect", k: 20 })).items.find((i) => i.key === "bitbucket:CI/sample-app#7");
  assert.ok(pr);
  assert.equal(pr.kind, "pr");
  assert.equal(pr.analysis, null);
});
