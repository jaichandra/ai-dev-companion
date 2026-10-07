const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const n = require("./notifications.js");

const MIN = 60 * 1000;
const T0 = Date.parse("2026-09-29T08:00:00Z");
const JOB = "3f2b8c1e-1111-4222-8333-444455556666";

function store(over = {}) {
  let clock = T0;
  let seq = 0;
  const s = n.createNotificationStore({ now: () => clock, idGen: () => `id-${++seq}`, ...over });
  return { s, tick: (ms) => (clock += ms) };
}

const conflict = (over = {}) => ({
  key: "conflicts:bitbucket:CI/sample-app#12:a:b",
  kind: "conflict",
  title: "Conflict on CI/sample-app #12: fix\nlogin",
  body: "Ready to review.",
  url: "https://bb.example/projects/CI/repos/sample-app/pull-requests/12",
  jobId: JOB,
  featureId: "resolve-conflict",
  watcher: "conflicts",
  scopeKey: "bitbucket:CI/sample-app#12",
  ...over,
});

test("cleanItem flattens titles, keeps only https links and valid job ids, and refuses junk", () => {
  const item = n.cleanItem(conflict({ url: "javascript:alert(1)", jobId: "not-a-uuid" }), T0, "x");
  assert.equal(item.title, "Conflict on CI/sample-app #12: fix login");
  assert.equal(item.url, null);
  assert.equal(item.jobId, undefined);
  assert.equal(n.cleanItem(conflict({ url: "https://u:p@bb.example/x" }), T0, "x").url, null);
  assert.throws(() => n.cleanItem({ ...conflict(), kind: "spam" }, T0, "x"), /Unknown notification kind/);
  assert.throws(() => n.cleanItem({ ...conflict(), title: "  " }, T0, "x"), /needs a title/);
  assert.throws(() => n.cleanItem({ ...conflict(), key: "" }, T0, "x"), /needs a key/);
  assert.equal(n.cleanItem(conflict({ title: "x".repeat(500) }), T0, "x").title.length, 200);
});

test("add dedupes by key: an unseen item is refreshed, a seen one never comes back", () => {
  const { s } = store();
  const first = s.add(conflict());
  assert.equal(first.id, "id-1");
  const again = s.add(conflict({ body: "Updated." }));
  assert.equal(again.id, "id-1");
  assert.equal(s.list().length, 1);
  assert.equal(s.list()[0].body, "Updated.");
  assert.equal(s.unseenCount(), 1);
  assert.equal(s.markSeen("all"), 1);
  assert.equal(s.add(conflict()), null);
  assert.equal(s.unseenCount(), 0);
  assert.equal(s.list({ includeSeen: true }).length, 1);
});

test("markSeen by id, open, and pruning after a week (seen) or two (unseen)", () => {
  const { s, tick } = store();
  const a = s.add(conflict({ key: "a" }));
  const b = s.add(conflict({ key: "b" }));
  assert.equal(s.markSeen([a.id, "nope"]), 1);
  const opened = s.open(b.id);
  assert.ok(opened.openedAt && opened.seenAt);
  assert.equal(s.open("nope"), null);
  s.add(conflict({ key: "c" }));
  tick(8 * 24 * 60 * MIN);
  assert.deepEqual(s.list({ includeSeen: true }).map((i) => i.key), ["c"]);
  tick(7 * 24 * 60 * MIN);
  assert.deepEqual(s.list({ includeSeen: true }), []);
});

test("planAnnouncement batches: one per interval, urgent at once, nothing in quiet hours", () => {
  const item = (id, over = {}) => ({ id, title: `T${id}`, body: `B${id}`, url: null, urgent: false, seenAt: null, announcedAt: null, updatedAt: T0, ...over });
  const opts = { now: T0, minIntervalMs: 30 * MIN, quiet: false };
  assert.equal(n.planAnnouncement([], opts), null);
  assert.deepEqual(n.planAnnouncement([item(1)], { ...opts, lastAnnouncedAt: 0 }), { ids: [1], urgent: false, title: "T1", message: "B1", url: null });
  assert.equal(n.planAnnouncement([item(1)], { ...opts, lastAnnouncedAt: T0 - 10 * MIN }), null, "too soon");
  const urgent = n.planAnnouncement([item(1), item(2, { urgent: true })], { ...opts, lastAnnouncedAt: T0 - MIN });
  assert.deepEqual(urgent.ids, [2, 1]);
  assert.equal(urgent.title, "2 things are ready for you");
  assert.equal(urgent.message, "• T2\n• T1");
  assert.equal(n.planAnnouncement([item(1, { urgent: true })], { ...opts, quiet: true }), null);
  assert.equal(n.planAnnouncement([item(1, { seenAt: T0 }), item(2, { announcedAt: T0 })], opts), null);
});

test("announce records what it announced; absorbPending hands quiet-hours items to the digest", () => {
  const { s, tick } = store();
  s.add(conflict({ key: "a" }));
  const first = s.announce({ minIntervalMs: 30 * MIN });
  assert.equal(first.title, "Conflict on CI/sample-app #12: fix login");
  assert.equal(first.url, "https://bb.example/projects/CI/repos/sample-app/pull-requests/12");
  assert.equal(s.announce({ minIntervalMs: 30 * MIN }), null, "nothing new");
  s.add(conflict({ key: "b" }));
  tick(5 * MIN);
  assert.equal(s.announce({ minIntervalMs: 30 * MIN }), null, "held until the interval passes");
  assert.equal(s.announce({ minIntervalMs: 0, quiet: true }), null);
  s.add({ key: "digest:1", kind: "digest", title: "Morning digest" });
  assert.equal(s.absorbPending().length, 1, "only the non-digest item");
  const digest = s.announce({ minIntervalMs: 0 });
  assert.equal(digest.title, "Morning digest");
});

test("the inbox file is written 0600 and read back; a broken file starts empty", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "inbox-"));
  try {
    const file = path.join(dir, "state", "notifications.json");
    const { s } = store({ file });
    s.add(conflict());
    assert.equal(fs.statSync(file).mode & 0o777, 0o600);
    const reloaded = n.createNotificationStore({ file, now: () => T0 });
    assert.equal(reloaded.list()[0].key, conflict().key);
    fs.writeFileSync(file, "{not json");
    assert.deepEqual(n.createNotificationStore({ file, now: () => T0 }).list(), []);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

const DAY = 24 * 60 * MIN;

test("only the newest 100 items are kept", () => {
  const { s, tick } = store();
  for (let i = 0; i < 105; i++) {
    s.add(conflict({ key: `k${i}` }));
    tick(1000);
  }
  const all = s.list({ limit: 200, includeSeen: true });
  assert.equal(all.length, 100);
  assert.equal(all[0].key, "k104");
  assert.equal(all.some((i) => i.key === "k0"), false);
  assert.equal(s.unseenCount(), 100);
});

test("expired items are gone from the count, get, add-refresh and announce the moment they expire", () => {
  const { s, tick } = store();
  const a = s.add(conflict({ key: "old" }));
  tick(15 * DAY);
  assert.equal(s.unseenCount(), 0);
  assert.equal(s.get(a.id), null);
  assert.equal(s.announce({ quiet: false, minIntervalMs: 0 }), null, "nothing expired is announced");
  assert.equal(s.open(a.id), null);
  const again = s.add(conflict({ key: "old" }));
  assert.notEqual(again.id, a.id, "an expired key is a new item, not a refresh");
});

test("announce: no interval means the default spacing, a future lastAnnouncedAt is clamped, and an urgent item bypasses the spacing", () => {
  const { s, tick } = store();
  s.add(conflict({ key: "a" }));
  assert.ok(s.announce({ quiet: false }), "first one goes out");
  tick(MIN);
  s.add(conflict({ key: "b" }));
  assert.equal(s.announce({ quiet: false }), null, "no minIntervalMs: the 30-minute default, not unlimited");
  assert.equal(s.announce({ quiet: false, minIntervalMs: NaN }), null);
  assert.equal(s.announce({ quiet: false, minIntervalMs: "0" }), null);
  s.add(conflict({ key: "c", urgent: true }));
  const urgent = s.announce({ quiet: false });
  assert.ok(urgent && urgent.urgent, "urgent goes out at once, through the store too");
  tick(31 * MIN);
  s.add(conflict({ key: "d" }));
  assert.ok(s.announce({ quiet: false }), "after 30 minutes the next batch goes");
});

test("lastAnnouncedAt persists and a future value from the file is clamped", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "inbox-last-"));
  try {
    const file = path.join(dir, "n.json");
    const { s } = store({ file });
    s.add(conflict({ key: "a" }));
    s.announce({ quiet: false, minIntervalMs: 0 });
    const saved = JSON.parse(fs.readFileSync(file, "utf8"));
    assert.equal(saved.lastAnnouncedAt, T0);
    const reloaded = n.createNotificationStore({ file, now: () => T0 + 5 * MIN, idGen: () => "z" });
    reloaded.add(conflict({ key: "b" }));
    assert.equal(reloaded.announce({ quiet: false, minIntervalMs: 30 * MIN }), null, "spacing survives a restart");
    saved.lastAnnouncedAt = T0 + 10 * DAY;
    fs.writeFileSync(file, JSON.stringify(saved));
    let clock = T0 + 60 * MIN;
    const future = n.createNotificationStore({ file, now: () => clock, idGen: () => "y" });
    future.add(conflict({ key: "c" }));
    assert.equal(future.announce({ quiet: false, minIntervalMs: 30 * MIN }), null, "clamped to now: treated as just announced");
    clock += 31 * MIN;
    assert.ok(future.announce({ quiet: false, minIntervalMs: 30 * MIN }), "a future timestamp silences the inbox for one interval, not for days");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a hand-edited file goes through the same cleaning; invalid items are dropped", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "inbox-bad-"));
  try {
    const file = path.join(dir, "n.json");
    const good = { ...conflict({ key: "good" }), id: "id-1", createdAt: T0, updatedAt: T0, seenAt: null };
    fs.writeFileSync(
      file,
      JSON.stringify({
        items: [
          good,
          { ...good, id: "id-2", key: "js", url: "javascript:alert(1)", title: "x".repeat(5000) },
          { ...good, id: "id-3", kind: "spam" },
          { ...good, id: "id-4", title: "" },
          { ...good, id: 5 },
          { ...good, id: "id-6", createdAt: "yesterday" },
          "junk",
          null,
        ],
        lastAnnouncedAt: "soon",
      }),
    );
    const s = n.createNotificationStore({ file, now: () => T0 });
    const items = s.list({ limit: 50 });
    assert.deepEqual(items.map((i) => i.key).sort(), ["good", "js"]);
    const js = items.find((i) => i.key === "js");
    assert.equal(js.url, null);
    assert.equal(js.title.length, 200);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a failed write is logged, the temp file is cleaned up, and the item is still in memory", () => {
  const removed = [];
  const failing = { ...fs, writeFileSync: () => { throw new Error("disk full"); }, rmSync: (p) => removed.push(p), mkdirSync: () => {} };
  const logs = [];
  const log = console.log;
  console.log = (m) => logs.push(m);
  try {
    const { s } = store({ file: "/no/such/dir/n.json", fsImpl: failing });
    assert.ok(s.add(conflict()));
    assert.equal(s.unseenCount(), 1);
  } finally {
    console.log = log;
  }
  assert.equal(removed.length, 1);
  assert.match(removed[0], /\.n\.json\.\d+\.tmp$/);
  assert.match(logs.join(""), /couldn't save the inbox: disk full/);
});

test("a refresh that becomes urgent is announced again; titles lose invisible and bidi characters", () => {
  const { s, tick } = store();
  s.add(conflict({ key: "a" }));
  assert.ok(s.announce({ quiet: false, minIntervalMs: 0 }));
  tick(MIN);
  s.add(conflict({ key: "a" }));
  assert.equal(s.announce({ quiet: false, minIntervalMs: 30 * MIN }), null, "a plain refresh is not news");
  s.add(conflict({ key: "a", urgent: true }));
  assert.ok(s.announce({ quiet: false, minIntervalMs: 30 * MIN }), "now urgent: tell again");
  const clean = n.cleanItem(conflict({ title: "Fi‮x​ed\u0085 it" }), T0, "x");
  assert.equal(clean.title, "Fixed it");
});
