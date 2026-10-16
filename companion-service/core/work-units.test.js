const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { openWorkUnits, unitKey, MAX_SESSIONS } = require("./work-units.js");

function setup() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "work-units-test-"));
  const file = path.join(dir, "state", "work-units.json");
  let t = 1000;
  const reg = openWorkUnits({ file, now: () => ++t });
  return { dir, file, reg, cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

test("upsert creates a unit, then fills in fields without blanking any", (t) => {
  const s = setup();
  t.after(s.cleanup);
  const a = s.reg.upsert({ repoPath: "/r/app", branch: "feature/X-1", dir: "/w/x1", issueKey: "X-1" });
  assert.equal(a.key, unitKey("/r/app", "feature/X-1"));
  const b = s.reg.upsert({ repoPath: "/r/app", branch: "feature/X-1", prKey: "bitbucket:PROJ/app#4" });
  assert.equal(b.dir, "/w/x1");
  assert.equal(b.issueKey, "X-1");
  assert.equal(b.prKey, "bitbucket:PROJ/app#4");
  assert.equal(b.createdAt, a.createdAt);
  assert.ok(b.updatedAt > a.updatedAt);
  assert.equal(s.reg.list().length, 1);
});

test("units are found by ticket and by PR, and survive a new handle on the same file", (t) => {
  const s = setup();
  t.after(s.cleanup);
  s.reg.upsert({ repoPath: "/r/app", branch: "b1", issueKey: "X-1", prKey: "p#1" });
  s.reg.upsert({ repoPath: "/r/other", branch: "b2", issueKey: "X-1" });
  const reopened = openWorkUnits({ file: s.file });
  assert.equal(reopened.findByIssue("X-1").length, 2);
  assert.equal(reopened.findByPr("p#1").length, 1);
  assert.equal(reopened.findByPr("nope").length, 0);
});

test("addSession records, refreshes and caps sessions, and ignores an unknown unit", (t) => {
  const s = setup();
  t.after(s.cleanup);
  const { key } = s.reg.upsert({ repoPath: "/r/app", branch: "b1" });
  s.reg.addSession(key, { id: "s-1", cwd: "/w/b1", feature: "address-review-comments" });
  s.reg.addSession(key, { id: "s-2" });
  const again = s.reg.addSession(key, { id: "s-1", feature: "resolve-conflict" });
  assert.deepEqual(again.sessions.map((x) => x.id), ["s-2", "s-1"]);
  assert.equal(again.sessions[1].feature, "resolve-conflict");
  for (let i = 0; i < MAX_SESSIONS + 5; i++) s.reg.addSession(key, { id: `n-${i}` });
  assert.equal(s.reg.get(key).sessions.length, MAX_SESSIONS);
  assert.equal(s.reg.addSession("/r/none#x", { id: "s-9" }), null);
  assert.throws(() => s.reg.addSession(key, { id: "bad id; rm" }), /session id/);
});

test("remove and prune drop units; prune keeps the live ones", (t) => {
  const s = setup();
  t.after(s.cleanup);
  const a = s.reg.upsert({ repoPath: "/r/app", branch: "a", dir: "/gone" });
  s.reg.upsert({ repoPath: "/r/app", branch: "b", dir: "/here" });
  const removed = s.reg.prune((u) => u.dir === "/here");
  assert.deepEqual(removed.map((u) => u.branch), ["a"]);
  assert.equal(s.reg.get(a.key), null);
  assert.equal(s.reg.remove(unitKey("/r/app", "b")), true);
  assert.equal(s.reg.remove(unitKey("/r/app", "b")), false);
});

test("a corrupt or foreign file reads as empty and is rewritten on the next change", (t) => {
  const s = setup();
  t.after(s.cleanup);
  fs.mkdirSync(path.dirname(s.file), { recursive: true });
  fs.writeFileSync(s.file, "{ not json");
  assert.deepEqual(s.reg.list(), []);
  s.reg.upsert({ repoPath: "/r/app", branch: "a" });
  assert.equal(JSON.parse(fs.readFileSync(s.file, "utf8")).version, 1);
  fs.writeFileSync(s.file, JSON.stringify({ version: 99, units: { x: {} } }));
  assert.deepEqual(s.reg.list(), []);
});

test("the file is private and no temp file is left behind", (t) => {
  const s = setup();
  t.after(s.cleanup);
  s.reg.upsert({ repoPath: "/r/app", branch: "a" });
  assert.equal(fs.statSync(s.file).mode & 0o777, 0o600);
  assert.deepEqual(fs.readdirSync(path.dirname(s.file)), ["work-units.json"]);
});

test("upsert needs a repo and a branch", (t) => {
  const s = setup();
  t.after(s.cleanup);
  assert.throws(() => s.reg.upsert({ repoPath: "/r/app" }), /repo path and a branch/);
});
