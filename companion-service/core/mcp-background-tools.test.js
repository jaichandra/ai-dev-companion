const test = require("node:test");
const assert = require("node:assert/strict");
const { listNotifications, assessPushRisk } = require("./mcp-background-tools.js");
const { TOOL_CATALOG, validateToolArgs, selectTools, inputSchemaFor } = require("./mcp-tools.js");
const { classifyMcpTool } = require("./mcp-tool-classifier.js");
const { validateFacts } = require("./risk-facts.js");
const { createNotificationStore } = require("./notifications.js");

const byName = (name) => TOOL_CATALOG.find((d) => d.name === name);
const FACTS = validateFacts({
  schema: "risk-facts/v1",
  generatedAt: "2026-09-29T02:00:00Z",
  tests: [],
  files: [{ path: "src/a.ts", regressions: [{ build: 7, tests: ["T"] }] }],
}).facts;

test("the new tools are read tools named with read verbs, and the core two are always offered", () => {
  for (const name of ["list_notifications", "get_change_risk", "get_digest"]) {
    assert.equal(byName(name).kind, "read", name);
    // The guard always denies the companion's own server to headless runs;
    // under any other name these would pass it as reads.
    assert.equal(classifyMcpTool(`mcp__other__${name}`), "allow", name);
    assert.equal(classifyMcpTool(`mcp__ai-companion__${name}`), "deny", name);
  }
  const names = selectTools({ enabledFeatureIds: [], externalServers: ["jira", "bitbucket"] }).map((d) => d.name);
  assert.ok(names.includes("list_notifications") && names.includes("get_change_risk"));
  assert.ok(!names.includes("get_digest"), "get_digest only while the digest feature is on");
  assert.deepEqual(inputSchemaFor(byName("get_change_risk")).required, ["files"]);
});

test("get_change_risk validates its file list and repo key", () => {
  const def = byName("get_change_risk");
  assert.deepEqual(validateToolArgs(def, { files: ["./src/a.ts", "src/a.ts"], repo: "CI/sample-app" }), { ok: true, value: { files: ["src/a.ts"], repo: "CI/sample-app" } });
  assert.match(validateToolArgs(def, { files: [] }).error, /at least one file/);
  assert.match(validateToolArgs(def, { files: ["../etc/passwd"] }).error, /repo-relative/);
  assert.match(validateToolArgs(def, { files: ["a"], repo: "sample-app" }).error, /PROJECT\/repo/);
});

test("assessPushRisk: skipped without a feed or when it can't be read, else the cited assessment", async () => {
  const none = await assessPushRisk({ files: ["src/a.ts"] }, { riskUrl: undefined, riskFacts: null });
  assert.equal(none.skipped, true);
  assert.match(none.reason, /isn't set up/);
  const down = await assessPushRisk({ files: ["src/a.ts"] }, { riskUrl: "https://facts/x", riskFacts: { get: async () => ({ ok: false, reason: "unreachable: HTTP 503" }) } });
  assert.match(down.reason, /can't be read right now \(unreachable: HTTP 503\)/);
  const r = await assessPushRisk(
    { files: ["src/a.ts"], repo: "CI/sample-app" },
    { riskUrl: "https://facts/x", riskFacts: { get: async () => ({ ok: true, facts: FACTS }) }, now: Date.parse("2026-09-29T09:00:00Z") },
  );
  assert.equal(r.skipped, false);
  assert.equal(r.level, "medium");
  assert.equal(r.repo, "CI/sample-app");
  assert.match(r.lines[0].text, /src\/a\.ts was in 1 regression: 1 build 7/);
  await assert.rejects(assessPushRisk({ files: ["/abs"] }, { riskUrl: "x" }), /repo-relative/);
});

test("listNotifications returns the inbox's items without internal fields", () => {
  const inbox = createNotificationStore({ now: () => 1, idGen: () => "id-1" });
  inbox.add({ key: "k", kind: "review-request", title: "Review requested: CI/sample-app #3" });
  const out = listNotifications(inbox, {});
  assert.equal(out.unseen, 1);
  assert.deepEqual(Object.keys(out.items[0]).sort(), ["body", "createdAt", "id", "jobId", "kind", "scopeKey", "seen", "title", "urgent", "url"]);
  assert.throws(() => listNotifications(undefined, {}), /isn't available/);
});
