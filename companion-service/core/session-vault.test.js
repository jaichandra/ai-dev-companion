const test = require("node:test");
const assert = require("node:assert/strict");
const util = require("node:util");
const fs = require("node:fs");
const path = require("node:path");
const { createSessionVault, pickCookie, originOf } = require("./session-vault.js");

const ORIGIN = "https://jira.example.com";

function fakeClock(start = 0) {
  let now = start;
  const clock = () => now;
  clock.advance = (ms) => (now += ms);
  return clock;
}

function vaultFor({ ttlMs = 30 * 60_000, allowed = [ORIGIN], clock } = {}) {
  let ttl = ttlMs;
  let origins = allowed;
  const vault = createSessionVault({
    ttlMs: () => ttl,
    allowedOrigins: () => origins,
    clock,
  });
  return {
    vault,
    setTtl: (ms) => (ttl = ms),
    setOrigins: (list) => (origins = list),
  };
}

test("record then get for an allowed origin returns the cookie", () => {
  const { vault } = vaultFor();
  assert.equal(vault.record(ORIGIN, "session=abc"), true);
  assert.equal(vault.get(ORIGIN), "session=abc");
});

test("record for a disallowed origin returns false and get returns undefined", () => {
  const disallowedOrigins = [
    "https://jira.example.com.evil.test",
    "http://jira.example.com",
    "https://jira.example.com:8443",
    null,
    "",
  ];
  for (const origin of disallowedOrigins) {
    const { vault } = vaultFor();
    assert.equal(vault.record(origin, "session=abc"), false, `expected record(${JSON.stringify(origin)}) to be false`);
    assert.equal(vault.get(origin), undefined, `expected get(${JSON.stringify(origin)}) to be undefined`);
    assert.equal(vault.size(), 0);
  }
});

test("both origin and cookie must be non-empty strings", () => {
  const { vault } = vaultFor();
  assert.equal(vault.record(ORIGIN, ""), false);
  assert.equal(vault.record(ORIGIN, undefined), false);
  assert.equal(vault.record(ORIGIN, null), false);
  assert.equal(vault.record(ORIGIN, 123), false);
  assert.equal(vault.size(), 0);
});

test("a cookie with CR or LF is refused", () => {
  const { vault } = vaultFor();
  assert.equal(vault.record(ORIGIN, "session=abc\r\nX-Injected: yes"), false);
  assert.equal(vault.record(ORIGIN, "session=abc\ninjected"), false);
  assert.equal(vault.size(), 0);
});

test("a cookie over 16 kB is refused", () => {
  const { vault } = vaultFor();
  const tooLong = "a".repeat(16 * 1024 + 1);
  assert.equal(vault.record(ORIGIN, tooLong), false);
  const atLimit = "a".repeat(16 * 1024);
  assert.equal(vault.record(ORIGIN, atLimit), true);
});

test("TTL: get at +29 min returns the cookie, at +31 min returns undefined and deletes the entry", () => {
  const clock = fakeClock();
  const { vault } = vaultFor({ ttlMs: 30 * 60_000, clock });

  assert.equal(vault.record(ORIGIN, "session=abc"), true);
  clock.advance(29 * 60_000);
  assert.equal(vault.get(ORIGIN), "session=abc");
  assert.equal(vault.size(), 1);

  clock.advance(2 * 60_000); // now +31 min
  assert.equal(vault.get(ORIGIN), undefined);
  assert.equal(vault.size(), 0, "the expired entry is deleted");
});

test("a later record refreshes `at`", () => {
  const clock = fakeClock();
  const { vault } = vaultFor({ ttlMs: 30 * 60_000, clock });

  vault.record(ORIGIN, "session=abc");
  clock.advance(29 * 60_000);
  vault.record(ORIGIN, "session=def"); // refreshes `at`
  clock.advance(29 * 60_000); // would be +58 min from start, but only +29 min since refresh
  assert.equal(vault.get(ORIGIN), "session=def");
});

test("ttlMs() === 0: record returns false and stores nothing", () => {
  const { vault } = vaultFor({ ttlMs: 0 });
  assert.equal(vault.record(ORIGIN, "session=abc"), false);
  assert.equal(vault.get(ORIGIN), undefined);
  assert.equal(vault.size(), 0);
});

test("if the TTL drops to 0 after a record, get returns undefined", () => {
  const { vault, setTtl } = vaultFor({ ttlMs: 30 * 60_000 });
  assert.equal(vault.record(ORIGIN, "session=abc"), true);
  setTtl(0);
  assert.equal(vault.get(ORIGIN), undefined);
});

test("the allowlist is read live on every call — a Settings change applies without a restart", () => {
  const { vault, setOrigins } = vaultFor({ allowed: [ORIGIN] });
  assert.equal(vault.record(ORIGIN, "session=abc"), true);
  setOrigins([]); // Settings removed this origin from the allowlist
  assert.equal(vault.get(ORIGIN), undefined, "no longer allowed, so get no longer returns it");
});

test("the TTL is read live on every call — a Settings change applies without a restart", () => {
  const clock = fakeClock();
  const { vault, setTtl } = vaultFor({ ttlMs: 30 * 60_000, clock });
  vault.record(ORIGIN, "session=abc");
  clock.advance(10 * 60_000);
  setTtl(5 * 60_000); // Settings shortened the TTL below the entry's current age
  assert.equal(vault.get(ORIGIN), undefined);
});

test("JSON.stringify(vault) does not contain the cookie", () => {
  const { vault } = vaultFor();
  vault.record(ORIGIN, "super-secret-session-cookie");
  const json = JSON.stringify(vault);
  assert.ok(!json.includes("super-secret-session-cookie"));
  assert.ok(!json.includes(ORIGIN), "origin isn't leaked either");
});

test("util.inspect(vault) does not contain the cookie, and shows SessionVault { entries: N }", () => {
  const { vault } = vaultFor();
  vault.record(ORIGIN, "super-secret-session-cookie");
  const inspected = util.inspect(vault);
  assert.ok(!inspected.includes("super-secret-session-cookie"));
  assert.equal(inspected, "SessionVault { entries: 1 }");
});

test("the module never calls console.log/warn/error during record/get", () => {
  const { vault } = vaultFor();
  const original = { log: console.log, warn: console.warn, error: console.error };
  const calls = [];
  console.log = (...args) => calls.push(["log", args]);
  console.warn = (...args) => calls.push(["warn", args]);
  console.error = (...args) => calls.push(["error", args]);
  try {
    vault.record(ORIGIN, "session=abc");
    vault.get(ORIGIN);
    vault.record("https://not-allowed.test", "session=xyz");
    vault.get("https://not-allowed.test");
  } finally {
    console.log = original.log;
    console.warn = original.warn;
    console.error = original.error;
  }
  assert.deepEqual(calls, []);
});

test("session-vault.js requires no fs module", () => {
  const source = fs.readFileSync(path.join(__dirname, "session-vault.js"), "utf8");
  assert.ok(!/require\(\s*["']fs["']\s*\)/.test(source), 'must not require("fs")');
  assert.ok(!/require\(\s*["']node:fs["']\s*\)/.test(source), 'must not require("node:fs")');
});

test("pickCookie returns the request cookie when its origin matches the site origin", () => {
  const { vault } = vaultFor();
  vault.record(ORIGIN, "vault-cookie");
  const requestAuth = { cookie: "request-cookie", origin: ORIGIN };
  assert.equal(pickCookie(ORIGIN, requestAuth, vault), "request-cookie");
});

test("pickCookie returns the vault's cookie when the request has none", () => {
  const { vault } = vaultFor();
  vault.record(ORIGIN, "vault-cookie");
  assert.equal(pickCookie(ORIGIN, undefined, vault), "vault-cookie");
  assert.equal(pickCookie(ORIGIN, {}, vault), "vault-cookie");
});

test("pickCookie returns the vault's cookie when the request has one for another origin", () => {
  const { vault } = vaultFor();
  vault.record(ORIGIN, "vault-cookie");
  const requestAuth = { cookie: "other-origin-cookie", origin: "https://other.example.com" };
  assert.equal(pickCookie(ORIGIN, requestAuth, vault), "vault-cookie");
});

test("pickCookie returns undefined when neither the request nor the vault has a cookie", () => {
  const { vault } = vaultFor();
  const requestAuth = { cookie: "other-origin-cookie", origin: "https://other.example.com" };
  assert.equal(pickCookie(ORIGIN, requestAuth, vault), undefined);
});

test("originOf strips trailing slashes and paths", () => {
  assert.equal(originOf("https://jira.example.com/"), "https://jira.example.com");
  assert.equal(originOf("https://x.test/sub/path"), "https://x.test");
});

test("originOf returns null for a non-URL", () => {
  assert.equal(originOf("not a url"), null);
});
