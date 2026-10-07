const test = require("node:test");
const assert = require("node:assert/strict");
const { createTokenCache } = require("./token-cache.js");

const UNDECRYPTABLE_MESSAGE = "Saved credentials can't be decrypted — re-enter your tokens in ✨ → ⚙ Settings.";

function undecryptableError() {
  const err = new Error(UNDECRYPTABLE_MESSAGE);
  err.code = "undecryptable";
  return err;
}

/** A minimal in-memory store, with call counters so tests can assert a
 * cache hit never reaches it, and switchable to `broken` (every read
 * throws undecryptable) or `failing` (every call throws some other
 * error) mid-test. */
function fakeStore(initial = {}) {
  const map = { ...initial };
  let mode = "ok";
  const calls = { get: 0, set: 0, remove: 0, list: 0 };
  return {
    calls,
    breakIt: () => (mode = "broken"),
    fixIt: () => (mode = "ok"),
    failWith: (err) => (mode = err),
    get(name) {
      calls.get++;
      if (mode === "broken") throw undecryptableError();
      if (mode !== "ok") throw mode;
      return map[name];
    },
    set(name, value) {
      calls.set++;
      if (mode !== "ok" && mode !== "broken") throw mode; // set() never throws undecryptable, by design.
      map[name] = value;
    },
    remove(name) {
      calls.remove++;
      if (mode === "broken") throw undecryptableError();
      if (mode !== "ok") throw mode;
      delete map[name];
    },
    list(name) {
      calls.list++;
      if (mode === "broken") throw undecryptableError();
      if (mode !== "ok") throw mode;
      return Object.keys(map);
    },
  };
}

function fakeClock(start = 0) {
  let now = start;
  const clock = () => now;
  clock.advance = (ms) => (now += ms);
  return clock;
}

test("getToken caches a hit for 60 seconds without calling the store again", () => {
  const store = fakeStore({ "jira.apiToken": "tok" });
  const clock = fakeClock();
  const cache = createTokenCache({ store, clock });

  assert.equal(cache.getToken("jira.apiToken"), "tok");
  assert.equal(store.calls.get, 1);
  clock.advance(59_999);
  assert.equal(cache.getToken("jira.apiToken"), "tok");
  assert.equal(store.calls.get, 1, "still within the 60s window — no second store.get");
});

test("getToken re-reads the store once the 60-second cache entry expires", () => {
  const store = fakeStore({ "jira.apiToken": "tok" });
  const clock = fakeClock();
  const cache = createTokenCache({ store, clock });

  cache.getToken("jira.apiToken");
  clock.advance(60_000);
  store.set("jira.apiToken", "rotated");
  cache.getToken("jira.apiToken");
  assert.equal(store.calls.get, 2);
  assert.equal(cache.getToken("jira.apiToken"), "rotated");
});

test("setToken immediately refreshes the cache — no stale read afterward", () => {
  const store = fakeStore({ "jira.apiToken": "old" });
  const clock = fakeClock();
  const cache = createTokenCache({ store, clock });

  cache.getToken("jira.apiToken"); // primes the cache with "old"
  cache.setToken("jira.apiToken", "new");
  assert.equal(cache.getToken("jira.apiToken"), "new");
  assert.equal(store.calls.get, 1, "the refreshed cache entry answered — store.get was never called again");
});

test("setToken(name, null) removes it, and the cache reflects that immediately", () => {
  const store = fakeStore({ "jira.apiToken": "tok" });
  const cache = createTokenCache({ store, clock: fakeClock() });

  cache.setToken("jira.apiToken", null);
  assert.equal(store.calls.remove, 1);
  assert.equal(cache.getToken("jira.apiToken"), undefined);
  assert.equal(store.calls.get, 0, "answered from cache, not the store");
});

test("getToken on an undecryptable store returns undefined and warns exactly once across repeated calls", () => {
  const store = fakeStore();
  store.breakIt();
  const clock = fakeClock();
  const warnings = [];
  const cache = createTokenCache({ store, clock, warn: (msg) => warnings.push(msg) });

  assert.equal(cache.getToken("jira.apiToken"), undefined);
  // Different name, and past the first entry's cache window — still hits
  // the store (mode stays "broken"), still only warns once.
  clock.advance(60_000);
  assert.equal(cache.getToken("jenkins.apiToken"), undefined);
  assert.equal(cache.getToken("jira.apiToken"), undefined);
  assert.equal(warnings.length, 1);
  assert.match(warnings[0], /re-enter your tokens/);
});

test("listNames on an undecryptable store returns [] and shares getToken's warn-once latch", () => {
  const store = fakeStore();
  store.breakIt();
  const warnings = [];
  const cache = createTokenCache({ store, clock: fakeClock(), warn: (msg) => warnings.push(msg) });

  assert.deepEqual(cache.listNames(), []);
  assert.equal(cache.getToken("jira.apiToken"), undefined);
  assert.deepEqual(cache.listNames(), []);
  assert.equal(warnings.length, 1, "one warning total, shared across listNames and getToken");
});

test("setToken(name, null) on an undecryptable store is a no-op, not a throw, and shares the latch", () => {
  const store = fakeStore();
  store.breakIt();
  const warnings = [];
  const cache = createTokenCache({ store, clock: fakeClock(), warn: (msg) => warnings.push(msg) });

  assert.doesNotThrow(() => cache.setToken("jira.apiToken", null));
  assert.equal(cache.getToken("jira.apiToken"), undefined);
  assert.equal(warnings.length, 1);
});

test("a successful set() clears the warn-once latch, so a later break warns again", () => {
  const store = fakeStore();
  store.breakIt();
  const warnings = [];
  const cache = createTokenCache({ store, clock: fakeClock(), warn: (msg) => warnings.push(msg) });

  cache.getToken("jira.apiToken");
  assert.equal(warnings.length, 1);

  store.fixIt();
  cache.setToken("jira.apiToken", "fresh-token"); // the real recovery path: re-enter in Settings.
  assert.equal(cache.getToken("jira.apiToken"), "fresh-token");
  assert.equal(warnings.length, 1, "no new warning — the store is fine again");

  store.breakIt();
  cache.getToken("a-name-never-cached-before"); // forces a real store.get, not a cache hit
  assert.equal(warnings.length, 2, "latch was reset, so this fresh break warns again");
});

test("a swallowed remove() failure does not reset the latch — no repeat warning on the next call", () => {
  const store = fakeStore();
  store.breakIt();
  const warnings = [];
  const cache = createTokenCache({ store, clock: fakeClock(), warn: (msg) => warnings.push(msg) });

  cache.getToken("jira.apiToken");
  assert.equal(warnings.length, 1);
  cache.setToken("jenkins.apiToken", null); // swallowed — store is still broken
  assert.deepEqual(cache.listNames(), [], "store is still broken");
  assert.equal(warnings.length, 1, "still just the one warning — remove() didn't fix anything");
});

test("a non-undecryptable error from get/set/remove/list is rethrown, not swallowed", () => {
  const boom = new Error("disk full");
  const store = fakeStore({ "jira.apiToken": "tok" });
  const cache = createTokenCache({ store, clock: fakeClock() });

  store.failWith(boom);
  assert.throws(() => cache.getToken("jira.apiToken"), /disk full/);
  assert.throws(() => cache.listNames(), /disk full/);
  assert.throws(() => cache.setToken("jira.apiToken", null), /disk full/);
  assert.throws(() => cache.setToken("jira.apiToken", "value"), /disk full/);
});
