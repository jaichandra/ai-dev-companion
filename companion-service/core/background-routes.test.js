// The inbox routes with a real in-memory inbox and a fake scheduler and
// history — no server, no disk.
const test = require("node:test");
const assert = require("node:assert/strict");
const { createNotificationStore } = require("./notifications.js");
const { createBackgroundRoutes } = require("./background-routes.js");
const { usedEvent } = require("./history-record.js");

const T0 = Date.parse("2026-09-29T08:00:00Z");
const JOB_ID = "3f2b8c1e-1111-4222-8333-444455556666";

function call(handler, req = {}) {
  let out;
  const res = {
    code: 200,
    status(c) {
      this.code = c;
      return this;
    },
    json(body) {
      out = { code: this.code, body };
    },
  };
  handler({ query: {}, body: {}, params: {}, ...req }, res);
  return out;
}

function setup({ quiet = false, history = true } = {}) {
  let seq = 0;
  const inbox = createNotificationStore({ now: () => T0, idGen: () => `00000000-0000-4000-8000-${String(++seq).padStart(12, "0")}` });
  const events = [];
  const jobs = {
    [JOB_ID]: { id: JOB_ID, featureId: "analyze-issue", scopeKey: "jira:PROJ-7", startedVia: "watcher", watcher: "assignedBugs", createdAt: T0 - 60_000 },
  };
  const routes = createBackgroundRoutes({
    inbox,
    scheduler: { status: () => ({ running: null, quiet }), quietNow: () => quiet },
    history: history ? { recordEvent: (e) => events.push(e) } : undefined,
    usedEvent,
    getJob: (id) => jobs[id],
    minIntervalMs: () => 30 * 60 * 1000,
    now: () => T0,
  });
  return { inbox, routes, events };
}

const item = (key, over = {}) => ({ key, kind: "analysis", title: `Analysis on the way for ${key}`, jobId: JOB_ID, ...over });

test("GET /notifications lists unseen items with the count and the scheduler's status; ?all=1 adds seen ones", () => {
  const { inbox, routes } = setup();
  const a = inbox.add(item("a"));
  inbox.add(item("b"));
  inbox.markSeen([a.id]);
  const r = call(routes.list);
  assert.equal(r.body.unseen, 1);
  assert.deepEqual(r.body.items.map((i) => i.key), ["b"]);
  assert.deepEqual(r.body.background, { running: null, quiet: false });
  assert.equal(call(routes.list, { query: { all: "1" } }).body.items.length, 2);
});

test("POST /notifications/seen takes ids or all, and refuses anything else", () => {
  const { inbox, routes } = setup();
  const a = inbox.add(item("a"));
  inbox.add(item("b"));
  assert.deepEqual(call(routes.seen, { body: { ids: [a.id] } }).body, { ok: true, seen: 1, unseen: 1 });
  assert.deepEqual(call(routes.seen, { body: { all: true } }).body, { ok: true, seen: 1, unseen: 0 });
  for (const body of [{}, { ids: "x" }, { ids: ["../x"] }, { ids: ["id-1"] }, { ids: [{ $ne: 1 }] }, { ids: new Array(101).fill("a") }, { all: "yes" }]) {
    assert.equal(call(routes.seen, { body }).code, 400, JSON.stringify(body).slice(0, 40));
  }
});

test("POST /notifications/announce plans one notification, and none in quiet hours", () => {
  const { inbox, routes } = setup();
  inbox.add(item("a"));
  const r = call(routes.announce);
  assert.equal(r.body.announce.title, "Analysis on the way for a");
  assert.equal(call(routes.announce).body.announce, null, "already announced");
  const q = setup({ quiet: true });
  q.inbox.add(item("a"));
  assert.equal(call(q.routes.announce).body.announce, null);
});

test("POST /notifications/:id/open marks it opened and records a pre-warmed job as used", () => {
  const { inbox, routes, events } = setup();
  const a = inbox.add(item("a"));
  const r = call(routes.open, { params: { id: a.id } });
  assert.equal(r.body.ok, true);
  assert.ok(r.body.item.openedAt);
  assert.equal(r.body.unseen, 0);
  assert.deepEqual(events.map((e) => [e.jobId, e.status, e.outcome, e.metrics.watcher]), [[JOB_ID, "opened", "used", "assignedBugs"]]);
  assert.equal(call(routes.open, { params: { id: "nope" } }).code, 404);
  assert.equal(call(routes.open, { params: { id: "../../x" } }).code, 404);
  const plain = inbox.add(item("c", { jobId: undefined }));
  call(routes.open, { params: { id: plain.id } });
  assert.equal(events.length, 1, "no job, no event");
  const off = setup({ history: false });
  const b = off.inbox.add(item("a"));
  assert.equal(call(off.routes.open, { params: { id: b.id } }).code, 200, "works with the history off");
});

test("opening a conflict item is not a use: only the approval records that", () => {
  const { inbox, routes, events } = setup();
  const conflictJob = "4f2b8c1e-1111-4222-8333-444455556666";
  const c = inbox.add({ key: "c1", kind: "conflict", title: "Conflict resolved", jobId: conflictJob, featureId: "resolve-conflict" });
  const r = call(routes.open, { params: { id: c.id } });
  assert.equal(r.code, 200);
  assert.deepEqual(events, [], "no `used` event for an approval-needing result");
});

test("opening the same item twice records one `used` event and keeps the first openedAt", () => {
  const { inbox, routes, events } = setup();
  const a = inbox.add(item("a"));
  const first = call(routes.open, { params: { id: a.id } });
  const second = call(routes.open, { params: { id: a.id } });
  assert.equal(events.length, 1);
  assert.equal(second.body.item.openedAt, first.body.item.openedAt);
});

test("POST /notifications/seen is strict: exactly one of ids / all, no extra keys, no arrays", () => {
  const { inbox, routes } = setup();
  const a = inbox.add(item("a"));
  for (const body of [{ all: true, ids: [a.id] }, { ids: [a.id], extra: 1 }, { all: true, extra: 1 }, [a.id], null, { all: false }, { all: 1 }]) {
    assert.equal(call(routes.seen, { body }).code, 400, JSON.stringify(body));
  }
  assert.equal(inbox.unseenCount(), 1);
  assert.equal(call(routes.seen, { body: { ids: [a.id] } }).code, 200);
});
