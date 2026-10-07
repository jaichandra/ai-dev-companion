// The LLM proxy request rules and client, with an injected fetch, sleep and
// clock — nothing here reaches the network or waits on a real timer.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const r = require("./llm-proxy-request.js");
const { createLlmProxyClient, LlmProxyError } = require("./llm-proxy-client.js");

const FIXTURES = path.join(__dirname, "fixtures", "llm-proxy", "openai-shape");
const fixture = (name) => JSON.parse(fs.readFileSync(path.join(FIXTURES, name), "utf8"));
const SETTINGS = r.llmProxySettings({}, { osUser: "jail" });
const TRIAGE_SCHEMA = {
  type: "object",
  required: ["worth", "reason"],
  properties: { worth: { type: "boolean" }, reason: { type: "string", maxLength: 300 } },
};


test("a model outside allowedModels is refused before anything is built", () => {
  const settings = { ...SETTINGS, chatModel: "gpt-5.1" };
  assert.throws(() => r.buildChatRequest({ settings, apiKey: "k", messages: [{ role: "user", content: "hi" }] }), /isn't one of the allowed models/);
  const embedSettings = { ...SETTINGS, embeddingModel: "text-embedding-3-large" };
  assert.throws(() => r.buildEmbedRequest({ settings: embedSettings, apiKey: "k", texts: ["a"] }), /allowed models/);
});


test("the chat fixture parses, with its JSON checked against the schema", () => {
  const out = r.parseChatResponse(fixture("chat.json"), { responseSchema: TRIAGE_SCHEMA });
  assert.deepEqual(out.value, { worth: true, reason: "The PR is yours and conflicts with master." });
  assert.equal(out.model, "qwen3.8-flash-next");
  assert.deepEqual(out.usage, { prompt: 212, completion: 19 });
  const fenced = { choices: [{ message: { content: 'Sure:\n```json\n{"worth": false, "reason": "draft"}\n```' } }] };
  assert.equal(r.parseChatResponse(fenced, { responseSchema: TRIAGE_SCHEMA }).value.worth, false);
  const wrong = { choices: [{ message: { content: '{"worth": "yes", "reason": "x"}' } }] };
  assert.throws(() => r.parseChatResponse(wrong, { responseSchema: TRIAGE_SCHEMA }), /reply.worth should be boolean/);
  assert.throws(() => r.parseChatResponse({ choices: [] }), /no message text/);
  assert.throws(() => r.parseChatResponse({ choices: [{ message: { content: "no json" } }] }, { responseSchema: TRIAGE_SCHEMA }), /isn't JSON/);
});

test("the embed fixture parses in input order", () => {
  assert.deepEqual(r.parseEmbedResponse(fixture("embed.json"), 2), [[0.1, 0.2, 0.3], [0.25, -0.5, 0.125]]);
  assert.throws(() => r.parseEmbedResponse(fixture("embed.json"), 3), /wrong number/);
  assert.throws(() => r.buildEmbedRequest({ settings: SETTINGS, apiKey: "k", texts: [] }), /1 to 64/);
});

test("retry and detection rules", () => {
  assert.equal(r.shouldRetry(429), true);
  assert.equal(r.shouldRetry(503), true);
  assert.equal(r.shouldRetry(400), false);
  assert.equal(r.shouldRetry(401), false);
  assert.equal(r.detectionState({ status: 200 }), "ready");
  assert.equal(r.detectionState({ status: 401 }), "key-rejected");
  assert.equal(r.detectionState({ status: 500 }), "error");
  assert.equal(r.detectionState({ networkError: true }), "unreachable");
});

/** A fake fetch answering from a queue of [status, body] (or an Error to throw). */
function fakeFetch(answers) {
  const calls = [];
  const fn = async (url, init) => {
    calls.push({ url, init });
    const next = answers.shift();
    if (next instanceof Error) throw next;
    const [status, body] = next;
    return { ok: status >= 200 && status < 300, status, text: async () => JSON.stringify(body) };
  };
  return { fn, calls };
}

function client(answers, over = {}) {
  const f = fakeFetch(answers);
  const sleeps = [];
  let clock = 1000;
  const c = createLlmProxyClient({
    getConfig: () => ({}),
    getApiKey: () => "sk-1",
    osUser: "jail",
    fetchImpl: f.fn,
    now: () => clock,
    sleep: async (ms) => sleeps.push(ms),
    timeoutSignal: () => undefined,
    ...over,
  });
  return { c, calls: f.calls, sleeps, tick: (ms) => (clock += ms) };
}

test("chat retries once on 429/5xx and never on other errors", async () => {
  const ok = client([[503, {}], [200, fixture("chat.json")]]);
  const out = await ok.c.chat({ messages: [{ role: "user", content: "hi" }], responseSchema: TRIAGE_SCHEMA });
  assert.equal(out.value.worth, true);
  assert.equal(ok.calls.length, 2);
  assert.deepEqual(ok.sleeps, [1000]);

  const twice = client([[500, {}], [502, {}]]);
  await assert.rejects(twice.c.chat({ messages: [{ role: "user", content: "hi" }] }), (err) => err instanceof LlmProxyError && err.status === 502);
  const denied = client([[401, {}]]);
  await assert.rejects(denied.c.chat({ messages: [{ role: "user", content: "hi" }] }), (err) => err.code === "key-rejected");
  assert.equal(denied.calls.length, 1);
  const offline = client([new TypeError("fetch failed")]);
  await assert.rejects(offline.c.chat({ messages: [{ role: "user", content: "hi" }] }), (err) => err.code === "unreachable");
  assert.equal(offline.calls.length, 1, "a network error isn't retried");
});



function fetchWithBodies(answers) {
  const calls = [];
  const cancelled = [];
  const fn = async (url, init) => {
    calls.push({ url, init });
    const status = answers.shift();
    return {
      ok: status >= 200 && status < 300,
      status,
      text: async () => JSON.stringify({ data: [] }),
      body: { cancel: async () => void cancelled.push(status) },
    };
  };
  return { fn, calls, cancelled };
}


test("every request refuses redirects and uses the 60 s / 10 s timeouts", async () => {
  const timeouts = [];
  const f = fakeFetch([[200, fixture("chat.json")], [200, fixture("embed.json")], [200, {}]]);
  const c = createLlmProxyClient({
    getConfig: () => ({}),
    getApiKey: () => "sk-1",
    osUser: "jail",
    fetchImpl: f.fn,
    timeoutSignal: (ms) => {
      timeouts.push(ms);
      return undefined;
    },
  });
  await c.chat({ messages: [{ role: "user", content: "hi" }] });
  await c.embed(["a", "b"]);
  await c.detect();
  assert.deepEqual(timeouts, [60000, 60000, 10000]);
  assert.deepEqual(f.calls.map((x) => x.init.redirect), ["error", "error", "error"]);
});

test("a chat model outside the allow-list never reaches fetch, through the client", async () => {
  const f = fakeFetch([[200, fixture("chat.json")]]);
  const c = createLlmProxyClient({
    getConfig: () => ({ llmProxy: { chatModel: "gpt-5.1" } }),
    getApiKey: () => "sk-1",
    osUser: "jail",
    fetchImpl: f.fn,
    timeoutSignal: () => undefined,
  });
  await assert.rejects(c.chat({ messages: [{ role: "user", content: "secret ticket text" }] }), /isn't one of the allowed models/);
  const e = createLlmProxyClient({
    getConfig: () => ({ llmProxy: { embeddingModel: "text-embedding-3-large" } }),
    getApiKey: () => "sk-1",
    fetchImpl: f.fn,
    timeoutSignal: () => undefined,
  });
  await assert.rejects(e.embed(["secret"]), /isn't one of the allowed models/);
  assert.equal(f.calls.length, 0);
});


test("answers the client won't read are cancelled, so no connection is left open", async () => {
  const f = fetchWithBodies([503, 500, 401, 200, 401]);
  const c = createLlmProxyClient({
    getConfig: () => ({}),
    getApiKey: () => "sk-1",
    osUser: "jail",
    fetchImpl: f.fn,
    sleep: async () => {},
    timeoutSignal: () => undefined,
  });
  await assert.rejects(c.chat({ messages: [{ role: "user", content: "hi" }] }), (e) => e.status === 500);
  assert.deepEqual(f.cancelled, [503, 500], "the retried 503 and the final 500");
  await assert.rejects(c.chat({ messages: [{ role: "user", content: "hi" }] }), (e) => e.code === "key-rejected");
  assert.deepEqual(f.cancelled, [503, 500, 401]);
  assert.equal((await c.detect()).state, "ready");
  assert.deepEqual(f.cancelled, [503, 500, 401, 200], "the detect answer's body is released too");
  assert.equal((await c.detect({ refresh: true })).state, "key-rejected");
  assert.deepEqual(f.cancelled, [503, 500, 401, 200, 401]);
});




test("the client name is a setting: default is this tool's own name, a custom one is sent, a bad one falls back", () => {
  assert.match(r.DEFAULT_USER_AGENT, /^ai-dev-companion\/\d/);
  const custom = r.llmProxySettings({ llmProxy: { userAgent: "QwenCode/1.0" } });
  assert.equal(custom.userAgent, "QwenCode/1.0");
  assert.equal(r.buildKeyCheckRequest({ settings: custom, apiKey: "sk-1" }).init.headers["user-agent"], "QwenCode/1.0");
  for (const bad of ["", "bad\r\nheader: x", "é", "x".repeat(101), 5]) {
    assert.equal(r.llmProxySettings({ llmProxy: { userAgent: bad } }).userAgent, r.DEFAULT_USER_AGENT, JSON.stringify(bad));
  }
});

test("classifyReply turns the proxy's refusals into fixed states without keeping its text", () => {
  const app = r.classifyReply(400, '{"error":{"type":"invalid_application_request","message":"Invalid request for qwencode application","param":"sk-secret"}}');
  assert.deepEqual(app, { state: "application", app: "qwencode" });
  assert.equal(r.classifyReply(400, '{"error":{"message":"You must pass a \'user\' json field to your request"}}').state, "user");
  assert.equal(r.classifyReply(403, '{"error":{"message":"You must pass a valid \'user\' field to your request"}}').state, "user");
  assert.equal(r.classifyReply(403, '{"error":{"message":"Unknown authorization credentials"}}').state, "key-rejected");
  assert.equal(r.classifyReply(401, "").state, "key-rejected");
  assert.equal(r.classifyReply(500, "boom").state, "error");
  assert.equal(r.classifyReply(400, undefined).state, "error");
  assert.ok(!JSON.stringify(app).includes("sk-secret"));
});

test("a certificate Node doesn't trust is its own detection state, not 'unreachable (VPN?)'", () => {
  const certError = Object.assign(new TypeError("fetch failed"), { cause: { code: "SELF_SIGNED_CERT_IN_CHAIN" } });
  assert.equal(r.tlsErrorCode(certError), "SELF_SIGNED_CERT_IN_CHAIN");
  assert.equal(r.tlsErrorCode(Object.assign(new TypeError("fetch failed"), { cause: { code: "ECONNREFUSED" } })), null);
  assert.equal(r.tlsErrorCode(new Error("boom")), null);
  assert.equal(r.tlsErrorCode(null), null);
  assert.equal(r.detectionState({ networkError: true, tlsError: true }), "tls");
  assert.equal(r.detectionState({ networkError: true }), "unreachable");
  assert.match(r.DETECTION_LABELS.tls, /certificate not trusted/);
});


test("detect and chat report an untrusted certificate as 'tls', with the fix in the message", async () => {
  const certError = () => Object.assign(new TypeError("fetch failed"), { cause: { code: "SELF_SIGNED_CERT_IN_CHAIN" } });
  const d = client([certError()]);
  const detected = await d.c.detect();
  assert.equal(detected.state, "tls");
  assert.match(detected.label, /certificate not trusted/);
  const chatting = client([certError()]);
  await assert.rejects(
    chatting.c.chat({ messages: [{ role: "user", content: "hi" }] }),
    (err) => err.code === "tls" && /NODE_USE_SYSTEM_CA=1/.test(err.message) && /SELF_SIGNED_CERT_IN_CHAIN/.test(err.message),
  );
  // A plain refused connection is still "unreachable" and names its cause.
  const refused = client([Object.assign(new TypeError("fetch failed"), { cause: { code: "ECONNREFUSED" } })]);
  await assert.rejects(
    refused.c.chat({ messages: [{ role: "user", content: "hi" }] }),
    (err) => err.code === "unreachable" && /ECONNREFUSED/.test(err.message),
  );
});

test("detect: a working key is ready; a key for another application, a bad user name, and a bad key are told apart", async () => {
  const good = client([[200, { data: [{ embedding: [0.1] }] }]]);
  assert.equal((await good.c.detect()).state, "ready");
  assert.match(good.calls[0].url, /\/v1\/embeddings$/);
  assert.equal(good.calls[0].init.headers["user-agent"], r.DEFAULT_USER_AGENT);

  const wrongApp = client([[400, { error: { type: "invalid_application_request", message: "Invalid request for qwencode application" } }]]);
  const a = await wrongApp.c.detect();
  assert.equal(a.state, "application");
  assert.match(a.label, /"qwencode" application/);

  const wrongUser = client([[403, { error: { message: "You must pass a valid 'user' field to your request" } }]]);
  assert.equal((await wrongUser.c.detect()).state, "user");

  const badKey = client([[403, { error: { message: "Unknown authorization credentials" } }]]);
  assert.equal((await badKey.c.detect()).state, "key-rejected");
});

test("chat and embed explain an application mismatch and a rejected user name", async () => {
  const wrongApp = client([[400, { error: { message: "Invalid request for qwencode application" } }]]);
  await assert.rejects(
    wrongApp.c.chat({ messages: [{ role: "user", content: "hi" }] }),
    (err) => err.code === "application" && /"qwencode" application/.test(err.message) && /Client name/.test(err.message),
  );
  const wrongUser = client([[403, { error: { message: "You must pass a valid 'user' field to your request" } }]]);
  await assert.rejects(wrongUser.c.embed(["x"]), (err) => err.code === "user");
});

test("an unexpected HTTP answer to the key check shows its status in the label", async () => {
  const odd = client([[404, {}]]);
  const detected = await odd.c.detect();
  assert.equal(detected.state, "error");
  assert.equal(detected.label, "error (HTTP 404)");
});

test("changing the client name, user name or embedding model re-checks at once instead of serving a stale answer", async () => {
  const cfg = { llmProxy: {} };
  const wrongApp = [400, { error: { message: "Invalid request for qwencode application" } }];
  const f = client([wrongApp, [200, { data: [{ embedding: [0.1] }] }], [200, { data: [{ embedding: [0.1] }] }]], { getConfig: () => cfg });
  assert.equal((await f.c.detect()).state, "application");
  assert.equal((await f.c.detect()).state, "application"); // cached: no new call
  assert.equal(f.calls.length, 1);
  cfg.llmProxy.userAgent = "QwenCode/1.0"; // as if saved in Settings
  assert.equal((await f.c.detect()).state, "ready");
  assert.equal(f.calls.length, 2);
  assert.equal(f.calls[1].init.headers["user-agent"], "QwenCode/1.0");
  cfg.llmProxy.user = "someone";
  await f.c.detect();
  assert.equal(f.calls.length, 3);
});

test("a reasoning model's real reply parses (leading newlines, reasoning_content ignored) and a budget-starved one says so", () => {
  const real = {
    model: "qwen3.8-27b",
    choices: [{ finish_reason: "stop", message: { role: "assistant", content: '\n\n{"worth": true, "reason": "test"}', reasoning_content: "We need to respond with JSON." } }],
    usage: { prompt_tokens: 69, completion_tokens: 53 },
  };
  const out = r.parseChatResponse(real, { responseSchema: TRIAGE_SCHEMA });
  assert.deepEqual(out.value, { worth: true, reason: "test" });
  assert.ok(!JSON.stringify(out).includes("We need to respond"));

  const starved = { model: "qwen3.8-27b", choices: [{ finish_reason: "length", message: { role: "assistant", content: "", reasoning_content: "thinking…" } }] };
  assert.throws(() => r.parseChatResponse(starved, { responseSchema: TRIAGE_SCHEMA }), /cut off before the answer/);
  assert.throws(() => r.parseChatResponse(starved), /cut off before the answer/);
});

test("the default and the triage token budgets leave room for the model's thinking", () => {
  const settings = r.llmProxySettings({});
  const body = JSON.parse(r.buildChatRequest({ settings, apiKey: "sk-1", messages: [{ role: "user", content: "hi" }] }).init.body);
  assert.ok(body.max_tokens >= 1024);
});
