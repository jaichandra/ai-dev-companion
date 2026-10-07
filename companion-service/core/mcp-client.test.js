const test = require("node:test");
const assert = require("node:assert/strict");
const { callTool } = require("./mcp-client.js");

const reply = (status, body, contentType = "application/json") => async () => ({
  status,
  ok: status >= 200 && status < 300,
  headers: { get: () => contentType },
  text: async () => (typeof body === "string" ? body : JSON.stringify(body)),
});
const toolResult = (payload, isError = false) => ({
  jsonrpc: "2.0",
  id: 1,
  result: { content: [{ type: "text", text: typeof payload === "string" ? payload : JSON.stringify(payload) }], isError },
});

test("sends a JSON-RPC tools/call with the bearer token and returns the parsed payload", async () => {
  let seen;
  const fetchImpl = async (url, init) => {
    seen = { url, init };
    return reply(200, toolResult([{ id: "j1" }]))();
  };
  const out = await callTool({ url: "http://127.0.0.1:8787/mcp", token: "tok", name: "list_jobs", args: { a: 1 }, fetchImpl });
  assert.deepEqual(out, [{ id: "j1" }]);
  assert.equal(seen.url, "http://127.0.0.1:8787/mcp");
  assert.equal(seen.init.headers.authorization, "Bearer tok");
  assert.match(seen.init.headers.accept, /application\/json/);
  const body = JSON.parse(seen.init.body);
  assert.equal(body.method, "tools/call");
  assert.deepEqual(body.params, { name: "list_jobs", arguments: { a: 1 } });
});

test("returns the text as-is when the tool's answer isn't JSON, and reads an SSE-framed answer", async () => {
  assert.equal(await callTool({ url: "u", token: "t", name: "n", fetchImpl: reply(200, toolResult("plain words")) }), "plain words");
  const sse = `event: message\ndata: ${JSON.stringify(toolResult({ ok: 1 }))}\n\n`;
  assert.deepEqual(await callTool({ url: "u", token: "t", name: "n", fetchImpl: reply(200, sse, "text/event-stream") }), { ok: 1 });
});

test("a tool error, a refused token and a down service each give a plain message", async () => {
  await assert.rejects(callTool({ url: "u", token: "t", name: "n", fetchImpl: reply(200, toolResult("Nothing is recorded under that key.", true)) }), /Nothing is recorded/);
  await assert.rejects(callTool({ url: "u", token: "t", name: "n", fetchImpl: reply(401, "no") }), /refused the token/);
  await assert.rejects(
    callTool({
      url: "http://127.0.0.1:8787/mcp",
      token: "t",
      name: "n",
      fetchImpl: async () => {
        const e = new TypeError("fetch failed");
        e.cause = { code: "ECONNREFUSED" };
        throw e;
      },
    }),
    /isn't running/,
  );
});
