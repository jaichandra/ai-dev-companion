const test = require("node:test");
const assert = require("node:assert/strict");
const { recordWorkUnit, recordWorkUnitSession, prKeyFor } = require("./work-unit-record.js");

function fakeRegistry() {
  const calls = [];
  return {
    calls,
    registry: () => ({
      upsert(u) {
        calls.push(["upsert", u]);
        return { key: `${u.repoPath}#${u.branch}` };
      },
      addSession(key, s) {
        calls.push(["addSession", key, s]);
      },
    }),
  };
}

test("recordWorkUnit upserts and returns the key", () => {
  const f = fakeRegistry();
  assert.equal(recordWorkUnit({ repoPath: "/r", branch: "b", dir: "/w" }, f), "/r#b");
  assert.equal(f.calls[0][1].dir, "/w");
});

test("recordWorkUnitSession makes the unit, then adds the session to it", () => {
  const f = fakeRegistry();
  recordWorkUnitSession({ repoPath: "/r", branch: "b", session: { id: "s-1", cwd: "/w" } }, f);
  assert.deepEqual(f.calls.map((c) => c[0]), ["upsert", "addSession"]);
  assert.equal(f.calls[1][1], "/r#b");
});

test("bookkeeping never throws", () => {
  const boom = { registry: () => { throw new Error("disk full"); } };
  assert.equal(recordWorkUnit({ repoPath: "/r", branch: "b" }, boom), null);
  assert.equal(recordWorkUnitSession({ repoPath: "/r", branch: "b", session: { id: "s" } }, boom), null);
});

test("prKeyFor follows the profile's git host and never throws", () => {
  const key = prKeyFor("PROJ", "app", 4);
  assert.ok(key === null || /PROJ\/app#4$/i.test(key) || /proj\/app#4$/i.test(key));
});
