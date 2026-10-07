const test = require("node:test");
const assert = require("node:assert/strict");
const stream = require("./claude-stream.js");

const j = (o) => JSON.stringify(o);
const INIT = j({ type: "system", subtype: "init", session_id: "s-1", cwd: "/w", tools: [] });
const readEvent = (file) =>
  j({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "ok" }, { type: "tool_use", id: "t1", name: "Read", input: { file_path: file } }] }, session_id: "s-1" });
const RESULT = j({
  type: "result",
  subtype: "success",
  is_error: false,
  result: "All done.",
  session_id: "s-1",
  permission_denials: [{ tool_name: "Bash", tool_use_id: "t2", tool_input: {} }],
});

test("parseStreamLine parses typed JSON objects and ignores everything else", () => {
  assert.equal(stream.parseStreamLine(INIT).subtype, "init");
  assert.equal(stream.parseStreamLine("not json"), null);
  assert.equal(stream.parseStreamLine('{"no":"type"}'), null);
  assert.equal(stream.parseStreamLine(""), null);
  assert.equal(stream.parseStreamLine(undefined), null);
});

test("labelForToolUse names the action, with paths relative to the working directory", () => {
  const label = (name, input) => stream.labelForToolUse({ type: "tool_use", name, input }, "/w");
  assert.equal(label("Read", { file_path: "/w/src/a.ts" }), "Reading src/a.ts");
  assert.equal(label("Read", { file_path: "/elsewhere/b.ts" }), "Reading /elsewhere/b.ts");
  assert.equal(label("Edit", { file_path: "/w/x.js" }), "Editing x.js");
  assert.equal(label("Grep", { pattern: "TODO" }), "Searching for TODO");
  assert.equal(label("Glob", { pattern: "**/*.ts" }), "Finding files **/*.ts");
  assert.equal(label("Bash", { command: "git diff --stat" }), "Running git diff --stat");
  assert.equal(label("mcp__acme-bitbucket__get_pull_request", {}), "Asking acme-bitbucket");
  assert.equal(label("WebFetch", { url: "https://secret.example/?t=abc" }), "Fetching a web page");
  assert.equal(label("SomethingNew", {}), "Using SomethingNew");
  assert.equal(stream.labelForToolUse({ type: "text" }, "/w"), null);
});

test("labels are one line, control characters stripped, capped at 80 characters", () => {
  const l = stream.labelForToolUse({ type: "tool_use", name: "Bash", input: { command: "echo a\nb\u0007c " + "x".repeat(200) } }, "/w");
  assert.equal(l.includes("\n"), false);
  assert.equal(l.includes("\u0007"), false);
  assert.ok(l.length <= 80);
  assert.ok(l.endsWith("…"));
});

test("the accumulator returns labels for tool calls and the final result at the end", () => {
  const acc = stream.createStreamAccumulator({ cwd: "/w" });
  assert.deepEqual(acc.feed(INIT), {});
  assert.deepEqual(acc.feed(readEvent("/w/a.ts")), { label: "Reading a.ts" });
  assert.deepEqual(acc.feed("garbage"), {});
  assert.equal(acc.result().sawResult, false);
  acc.feed(RESULT);
  assert.deepEqual(acc.result(), {
    sawResult: true,
    text: "All done.",
    sessionId: "s-1",
    permissionDenials: ["Bash"],
    isError: false,
  });
});

test("createThrottle lets one call through per interval", () => {
  let t = 1000;
  const throttle = stream.createThrottle(1000, () => t);
  const seen = [];
  throttle(() => seen.push("a"));
  throttle(() => seen.push("b"));
  t = 1999;
  throttle(() => seen.push("c"));
  t = 2000;
  throttle(() => seen.push("d"));
  assert.deepEqual(seen, ["a", "d"]);
});
