const test = require("node:test");
const assert = require("node:assert/strict");
const { PassThrough } = require("node:stream");
const { createStdioBridge, runStdio, requestIds } = require("./mcp-stdio.js");

const URL = "http://127.0.0.1:8787/mcp";

function response(status, body, type = "application/json") {
  return { status, ok: status >= 200 && status < 300, text: async () => body, headers: { get: () => type } };
}

function bridge({ reply, token = "tok" } = {}) {
  const written = [];
  const sent = [];
  const b = createStdioBridge({
    url: URL,
    getToken: () => token,
    write: (m) => written.push(m),
    fetchImpl: async (url, init) => {
      sent.push({ url, init });
      if (reply instanceof Error) throw reply;
      return typeof reply === "function" ? reply(init) : reply;
    },
  });
  return { b, written, sent };
}

const REQ = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" });

test("forwards a request with the bearer token and writes the answer back", async () => {
  const t = bridge({ reply: response(200, JSON.stringify({ jsonrpc: "2.0", id: 1, result: { tools: [] } })) });
  await t.b.handleLine(REQ);
  assert.equal(t.sent[0].url, URL);
  assert.equal(t.sent[0].init.headers.authorization, "Bearer tok");
  assert.equal(t.sent[0].init.body, REQ);
  assert.deepEqual(t.written, [{ jsonrpc: "2.0", id: 1, result: { tools: [] } }]);
});

test("an event-stream answer and a notification's empty 202 both work", async () => {
  const sse = bridge({ reply: response(200, 'event: message\ndata: {"jsonrpc":"2.0","id":1,"result":{}}\n\n', "text/event-stream") });
  await sse.b.handleLine(REQ);
  assert.deepEqual(sse.written, [{ jsonrpc: "2.0", id: 1, result: {} }]);
  const note = bridge({ reply: response(202, "") });
  await note.b.handleLine(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }));
  assert.deepEqual(note.written, []);
});

test("the service being down answers each request with 'companion service not running'", async () => {
  const down = Object.assign(new Error("fetch failed"), { cause: { code: "ECONNREFUSED" } });
  const t = bridge({ reply: down });
  await t.b.handleLine(REQ);
  assert.equal(t.written.length, 1);
  assert.equal(t.written[0].id, 1);
  assert.match(t.written[0].error.message, /^companion service not running \(nothing is listening on port 8787\)/);
  const note = bridge({ reply: down });
  await note.b.handleLine(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }));
  assert.deepEqual(note.written, [], "a notification gets no answer, even an error");
});

test("no token, a refused token, an HTTP error, junk in and junk out", async () => {
  const none = bridge({ token: "", reply: response(200, "{}") });
  await none.b.handleLine(REQ);
  assert.match(none.written[0].error.message, /No MCP token/);
  assert.equal(none.sent.length, 0, "nothing is sent without a token");
  const refused = bridge({ reply: response(401, "") });
  await refused.b.handleLine(REQ);
  assert.match(refused.written[0].error.message, /refused the MCP token/);
  const http = bridge({ reply: response(500, "oops") });
  await http.b.handleLine(REQ);
  assert.match(http.written[0].error.message, /HTTP 500/);
  const junkIn = bridge({ reply: response(200, "{}") });
  await junkIn.b.handleLine("not json");
  assert.deepEqual(junkIn.written, [{ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } }]);
  await junkIn.b.handleLine("   ");
  assert.equal(junkIn.written.length, 1, "a blank line is ignored");
  const junkOut = bridge({ reply: response(200, "<html>") });
  await junkOut.b.handleLine(REQ);
  assert.match(junkOut.written[0].error.message, /isn't JSON-RPC/);
});

test("requestIds finds the requests in a message or a batch", () => {
  assert.deepEqual(requestIds({ jsonrpc: "2.0", id: 3, method: "x" }), [3]);
  assert.deepEqual(requestIds([{ id: 1, method: "a" }, { method: "n" }, { id: 2, result: {} }]), [1]);
  assert.deepEqual(requestIds(null), []);
});

const OK = JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} });

test("a body that cannot be read answers the request with an error and the bridge lives on", async () => {
  const boom = { status: 200, ok: true, headers: { get: () => "application/json" }, text: async () => { throw new Error("socket hang up Bearer tok"); } };
  const t = bridge({ reply: boom });
  await t.b.handleLine(REQ);
  assert.equal(t.written.length, 1);
  assert.equal(t.written[0].id, 1);
  assert.match(t.written[0].error.message, /socket hang up/);
  assert.ok(!t.written[0].error.message.includes("tok"), "no token in the message");
  assert.equal(t.written[0].error.code, -32000);
});

test("a write that throws never breaks the bridge", async () => {
  const b = createStdioBridge({ url: URL, getToken: () => "tok", write: () => { throw new Error("EPIPE"); }, fetchImpl: async () => response(200, OK) });
  await b.handleLine(REQ);
});

test("a request the service didn't answer gets an error; a JSON-RPC error body is passed through", async () => {
  const empty = bridge({ reply: response(202, "") });
  await empty.b.handleLine(REQ);
  assert.equal(empty.written.length, 1);
  assert.equal(empty.written[0].id, 1);
  assert.match(empty.written[0].error.message, /no answer/);
  const rpc = { jsonrpc: "2.0", id: null, error: { code: -32600, message: "Invalid Request: bad session" } };
  const passed = bridge({ reply: response(400, JSON.stringify(rpc)) });
  await passed.b.handleLine(REQ);
  assert.deepEqual(passed.written[0], rpc);
  assert.equal(passed.written[1].id, 1, "the request itself is still answered");
  const batch = bridge({ reply: response(200, JSON.stringify([{ jsonrpc: "2.0", id: 1, result: {} }])) });
  await batch.b.handleLine(JSON.stringify([{ jsonrpc: "2.0", id: 1, method: "a" }, { jsonrpc: "2.0", id: 2, method: "b" }]));
  assert.deepEqual(batch.written.map((m) => [m.id, Boolean(m.error)]), [[1, false], [2, true]]);
});

test("an answer over the size cap is refused, declared or not", async () => {
  const big = createStdioBridge({ url: URL, getToken: () => "tok", write: (m) => big.written.push(m), maxBytes: 10, fetchImpl: async () => response(200, OK) });
  big.written = [];
  await big.handleLine(REQ);
  assert.match(big.written[0].error.message, /too large/);
  const declared = createStdioBridge({
    url: URL, getToken: () => "tok", write: (m) => declared.written.push(m), maxBytes: 10,
    fetchImpl: async () => ({ ...response(200, "{}"), headers: { get: (h) => (h === "content-length" ? "999" : "application/json") } }),
  });
  declared.written = [];
  await declared.handleLine(REQ);
  assert.match(declared.written[0].error.message, /too large/);
});

test("a timeout is named as such, and the default request timeout is 60 s", async () => {
  const { REQUEST_TIMEOUT_MS } = require("./mcp-stdio.js");
  assert.equal(REQUEST_TIMEOUT_MS, 60000);
  const t = bridge({ reply: Object.assign(new Error("aborted"), { name: "TimeoutError" }) });
  await t.b.handleLine(REQ);
  assert.match(t.written[0].error.message, /didn't answer within 60 seconds/);
});

function harness({ reply, token = "tok" } = {}) {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  const out = [];
  stdout.on("data", (d) => out.push(d.toString()));
  const b = createStdioBridge({ url: URL, getToken: () => token, fetchImpl: async (u, init) => reply(JSON.parse(init.body)) });
  const done = runStdio({ stdin, stdout, stderr, bridge: b });
  const lines = () => out.join("").split("\n").filter(Boolean).map((l) => JSON.parse(l));
  return { stdin, stdout, stderr, done, lines };
}
const echo = (msg) => response(200, JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { echoed: msg.method } }));

test("runStdio handles several lines in one chunk and a line split across chunks, in order", async () => {
  const h = harness({ reply: echo });
  h.stdin.write('{"jsonrpc":"2.0","id":1,"method":"a"}\n{"jsonrpc":"2.0","id":2,"me');
  h.stdin.write('thod":"b"}\r\n');
  h.stdin.end('{"jsonrpc":"2.0","id":3,"method":"c"}');
  assert.equal(await h.done, 0);
  assert.deepEqual(h.lines().map((m) => m.result.echoed), ["a", "b", "c"], "a last line without a newline is still handled");
});

test("runStdio waits for an in-flight request after stdin ends", async () => {
  let release;
  const gate = new Promise((r) => (release = r));
  const h = harness({ reply: async (msg) => { await gate; return echo(msg); } });
  h.stdin.end('{"jsonrpc":"2.0","id":1,"method":"slow"}\n');
  let finished = false;
  h.done.then(() => (finished = true));
  await new Promise((r) => setTimeout(r, 30));
  assert.equal(finished, false, "not exited while a request is in flight");
  release();
  assert.equal(await h.done, 0);
  assert.deepEqual(h.lines().map((m) => m.id), [1]);
});

test("runStdio keeps going after a request whose body cannot be read", async () => {
  const bad = { status: 200, ok: true, headers: { get: () => "application/json" }, text: async () => { throw new Error("reset"); } };
  const h = harness({ reply: (msg) => (msg.id === 1 ? bad : echo(msg)) });
  h.stdin.end('{"jsonrpc":"2.0","id":1,"method":"a"}\n{"jsonrpc":"2.0","id":2,"method":"b"}\n');
  assert.equal(await h.done, 0);
  const got = h.lines();
  assert.equal(got[0].error.code, -32000);
  assert.equal(got[1].result.echoed, "b");
});

test("runStdio exits cleanly (0) when stdout closes with EPIPE, and never throws from a write", async () => {
  const h = harness({ reply: echo });
  h.stdout.emit("error", Object.assign(new Error("write EPIPE"), { code: "EPIPE" }));
  assert.equal(await h.done, 0);
  h.stdin.write('{"jsonrpc":"2.0","id":1,"method":"a"}\n');
  const throwing = { write() { throw new Error("EPIPE"); }, on() {} };
  const stdin = new PassThrough();
  const done = runStdio({ stdin, stdout: throwing, stderr: new PassThrough(), bridge: createStdioBridge({ url: URL, getToken: () => "tok", fetchImpl: async () => response(200, OK) }) });
  stdin.end(REQ + "\n");
  assert.equal(await done, 0);
});
