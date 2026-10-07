const test = require("node:test");
const assert = require("node:assert/strict");
const { createHostGuard } = require("./host-guard.js");

function run(hostHeader, port = 8787) {
  const guard = createHostGuard(() => port);
  const out = { nextCalled: false, status: undefined, body: undefined };
  guard(
    { headers: { host: hostHeader } },
    {
      status(code) {
        out.status = code;
        return this;
      },
      json(body) {
        out.body = body;
      },
    },
    () => {
      out.nextCalled = true;
    },
  );
  return out;
}

test("lets requests addressed to 127.0.0.1 or localhost on our port through", () => {
  assert.equal(run("127.0.0.1:8787").nextCalled, true);
  assert.equal(run("localhost:8787").nextCalled, true);
});

test("refuses any other Host header with a 403, before the route runs", () => {
  for (const host of ["evil.example.com:8787", "127.0.0.1:9999", "127.0.0.1", "", undefined, "127.0.0.1:8787.evil.com"]) {
    const r = run(host);
    assert.equal(r.nextCalled, false, String(host));
    assert.equal(r.status, 403, String(host));
    assert.match(r.body.error, /127\.0\.0\.1|localhost/);
  }
});

test("follows the current port", () => {
  assert.equal(run("127.0.0.1:9000", 9000).nextCalled, true);
  assert.equal(run("127.0.0.1:8787", 9000).nextCalled, false);
});
