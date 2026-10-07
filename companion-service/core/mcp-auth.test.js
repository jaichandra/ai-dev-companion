const test = require("node:test");
const assert = require("node:assert/strict");
const mcpAuth = require("./mcp-auth.js");

const TOKEN = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

// ---- bearerMatches ----

test("bearerMatches accepts the exact token", () => {
  assert.equal(mcpAuth.bearerMatches(`Bearer ${TOKEN}`, TOKEN), true);
});

test("bearerMatches accepts a lowercase bearer scheme", () => {
  assert.equal(mcpAuth.bearerMatches(`bearer ${TOKEN}`, TOKEN), true);
});

test("bearerMatches rejects a wrong token of the same length", () => {
  const wrong = `${TOKEN.slice(0, -1)}X`;
  assert.equal(wrong.length, TOKEN.length);
  assert.equal(mcpAuth.bearerMatches(`Bearer ${wrong}`, TOKEN), false);
});

test("bearerMatches rejects a token of a different length", () => {
  assert.equal(mcpAuth.bearerMatches(`Bearer ${TOKEN}x`, TOKEN), false);
  assert.equal(mcpAuth.bearerMatches(`Bearer ${TOKEN.slice(1)}`, TOKEN), false);
});

test("bearerMatches rejects two spaces, another scheme, and a missing header", () => {
  assert.equal(mcpAuth.bearerMatches("bearer  x", "x"), false);
  assert.equal(mcpAuth.bearerMatches(`Bearer  ${TOKEN}`, TOKEN), false);
  assert.equal(mcpAuth.bearerMatches("Basic x", "x"), false);
  assert.equal(mcpAuth.bearerMatches(undefined, TOKEN), false);
});

test("bearerMatches is false whenever no token is expected", () => {
  assert.equal(mcpAuth.bearerMatches("Bearer ", ""), false);
  assert.equal(mcpAuth.bearerMatches("Bearer x", ""), false);
  assert.equal(mcpAuth.bearerMatches("Bearer undefined", undefined), false);
  assert.equal(mcpAuth.bearerMatches(undefined, undefined), false);
});

test("bearerMatches never throws on a 10 kB header", () => {
  const huge = `Bearer ${"a".repeat(10 * 1024)}`;
  assert.doesNotThrow(() => mcpAuth.bearerMatches(huge, TOKEN));
  assert.equal(mcpAuth.bearerMatches(huge, TOKEN), false);
  assert.equal(mcpAuth.bearerMatches("x".repeat(10 * 1024), TOKEN), false);
});

// ---- hostAllowed ----

test("hostAllowed accepts 127.0.0.1 and localhost on the service's own port", () => {
  assert.equal(mcpAuth.hostAllowed("127.0.0.1:8787", 8787), true);
  assert.equal(mcpAuth.hostAllowed("localhost:8787", 8787), true);
});

test("hostAllowed rejects another port, another host, no port, no header, and a suffixed host", () => {
  assert.equal(mcpAuth.hostAllowed("127.0.0.1:9999", 8787), false);
  assert.equal(mcpAuth.hostAllowed("evil.test:8787", 8787), false);
  assert.equal(mcpAuth.hostAllowed("127.0.0.1", 8787), false);
  assert.equal(mcpAuth.hostAllowed(undefined, 8787), false);
  assert.equal(mcpAuth.hostAllowed("127.0.0.1:8787.evil.test", 8787), false);
});

// ---- generateMcpToken ----

test("generateMcpToken returns 43 base64url characters, different each call", () => {
  const a = mcpAuth.generateMcpToken();
  const b = mcpAuth.generateMcpToken();
  assert.equal(a.length, 43);
  assert.match(a, /^[A-Za-z0-9_-]{43}$/);
  assert.match(b, /^[A-Za-z0-9_-]{43}$/);
  assert.notEqual(a, b);
});

test("generateMcpToken draws its 32 bytes from the injected randomBytes", () => {
  let asked;
  const token = mcpAuth.generateMcpToken((n) => {
    asked = n;
    return Buffer.alloc(n, 0xff);
  });
  assert.equal(asked, 32);
  assert.equal(token, Buffer.alloc(32, 0xff).toString("base64url"));
});
