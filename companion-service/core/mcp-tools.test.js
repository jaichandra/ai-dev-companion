const test = require("node:test");
const assert = require("node:assert/strict");
const {
  TOOL_CATALOG,
  PARAM_TYPES,
  inputSchemaFor,
  validateToolArgs,
  selectTools,
  capToolOutput,
} = require("./mcp-tools.js");
const registry = require("./feature-registry.js");

function byName(name) {
  const def = TOOL_CATALOG.find((d) => d.name === name);
  assert.ok(def, `no tool def named ${name}`);
  return def;
}

// ---- catalog shape ----

test("catalog names are unique and match the tool-name shape", () => {
  const names = TOOL_CATALOG.map((d) => d.name);
  assert.equal(new Set(names).size, names.length);
  for (const name of names) {
    assert.match(name, /^[a-z][a-z0-9_]{2,63}$/, `bad name: ${name}`);
  }
});

test("every featureId is a real registry id or null", () => {
  const known = new Set(registry.allFeatureIds());
  for (const def of TOOL_CATALOG) {
    if (def.featureId === null) continue;
    assert.ok(known.has(def.featureId), `unknown featureId: ${def.featureId}`);
  }
});

test("every def has a description of at least 40 chars", () => {
  for (const def of TOOL_CATALOG) {
    assert.ok(
      typeof def.description === "string" && def.description.length >= 40,
      `short description for ${def.name}`,
    );
  }
});

test("catalog build order is domain tools first, generic readers last", () => {
  const genericFlags = TOOL_CATALOG.map((d) => d.generic);
  const firstGenericIndex = genericFlags.indexOf(true);
  if (firstGenericIndex === -1) return;
  for (let i = firstGenericIndex; i < genericFlags.length; i++) {
    assert.equal(genericFlags[i], true, "a non-generic tool appears after a generic one");
  }
});

// ---- read-only policy ----

test("kind:start tools are exactly analyze_issue", () => {
  const startNames = TOOL_CATALOG.filter((d) => d.kind === "start").map((d) => d.name);
  assert.deepEqual(startNames, ["analyze_issue"]);
});

test("pending-start tools are exactly the two write-shaped features", () => {
  const pendingNames = TOOL_CATALOG.filter((d) => d.kind === "pending-start").map((d) => d.name);
  assert.deepEqual(pendingNames, ["start_resolve_conflict", "start_address_review_comments"]);
});

test("no tool name contains a mutating verb", () => {
  const banned = ["push", "post", "comment_create", "merge", "approve", "delete", "write"];
  for (const def of TOOL_CATALOG) {
    for (const word of banned) {
      assert.ok(!def.name.includes(word), `${def.name} contains banned word ${word}`);
    }
  }
});

// ---- inputSchemaFor ----

test("inputSchemaFor(start_resolve_conflict) gives the exact expected schema", () => {
  const schema = inputSchemaFor(byName("start_resolve_conflict"));
  assert.deepEqual(schema, {
    type: "object",
    properties: {
      project: { type: "string", maxLength: 100 },
      repo: { type: "string", maxLength: 100 },
      prId: { type: "integer", minimum: 1 },
    },
    required: ["project", "repo", "prId"],
    additionalProperties: false,
  });
});


test("inputSchemaFor marks optional args as not required", () => {
  const schema = inputSchemaFor(byName("analyze_issue"));
  assert.deepEqual(schema.required, ["issueKey"]);
  assert.ok("force" in schema.properties);
});

test("inputSchemaFor gives featureId params an enum of the real registry ids", () => {
  const schema = inputSchemaFor(byName("list_jobs"));
  assert.deepEqual(schema.properties.featureId.enum, registry.allFeatureIds());
});

// ---- validateToolArgs: accepts ----

test("validateToolArgs accepts good args for start_resolve_conflict", () => {
  const result = validateToolArgs(byName("start_resolve_conflict"), { project: "CI", repo: "sample-app", prId: 12 });
  assert.deepEqual(result, { ok: true, value: { project: "CI", repo: "sample-app", prId: 12 } });
});

test("validateToolArgs coerces a numeric-string prId to a number", () => {
  const result = validateToolArgs(byName("start_resolve_conflict"), { project: "CI", repo: "sample-app", prId: "42" });
  assert.equal(result.ok, true);
  assert.equal(result.value.prId, 42);
});

test("validateToolArgs uppercases an issue key", () => {
  const result = validateToolArgs(byName("get_issue_analysis"), { issueKey: "proj-7" });
  assert.equal(result.ok, true);
  assert.equal(result.value.issueKey, "PROJ-7");
});


test("validateToolArgs accepts an omitted optional arg", () => {
  const result = validateToolArgs(byName("analyze_issue"), { issueKey: "PROJ-1" });
  assert.equal(result.ok, true);
  assert.equal(result.value.issueKey, "PROJ-1");
  assert.ok(!("force" in result.value));
});

// ---- validateToolArgs: rejects ----

test("validateToolArgs rejects a path-traversal project", () => {
  const result = validateToolArgs(byName("start_resolve_conflict"), { project: "../x", repo: "sample-app", prId: 1 });
  assert.equal(result.ok, false);
  assert.match(result.error, /project/);
});

test("validateToolArgs rejects a repo with a space", () => {
  const result = validateToolArgs(byName("start_resolve_conflict"), { project: "CI", repo: "a b", prId: 1 });
  assert.equal(result.ok, false);
  assert.match(result.error, /repo/);
});

for (const badPrId of [0, -1, 1.5, "1e3"]) {
  test(`validateToolArgs rejects prId ${JSON.stringify(badPrId)}`, () => {
    const result = validateToolArgs(byName("start_resolve_conflict"), { project: "CI", repo: "sample-app", prId: badPrId });
    assert.equal(result.ok, false);
    assert.match(result.error, /prId/);
  });
}

test("validateToolArgs rejects an issue key with no ticket number", () => {
  const result = validateToolArgs(byName("get_issue_analysis"), { issueKey: "PROJ" });
  assert.equal(result.ok, false);
  assert.match(result.error, /issueKey/);
});

test("validateToolArgs rejects a malformed jobId", () => {
  const result = validateToolArgs(byName("get_job"), { jobId: "x" });
  assert.equal(result.ok, false);
  assert.match(result.error, /jobId/);
});

test("validateToolArgs rejects a malformed scopeKey", () => {
  const result = validateToolArgs(byName("list_jobs"), { scopeKey: "jira:PROJ-1;rm" });
  assert.equal(result.ok, false);
  assert.match(result.error, /scopeKey/);
});

test("validateToolArgs rejects an unknown feature id", () => {
  const result = validateToolArgs(byName("list_jobs"), { featureId: "not-a-real-feature" });
  assert.equal(result.ok, false);
  assert.match(result.error, /featureId/);
});


test("validateToolArgs rejects a missing required arg", () => {
  const result = validateToolArgs(byName("get_job"), {});
  assert.equal(result.ok, false);
  assert.match(result.error, /jobId/);
});


test("validateToolArgs never echoes a whole huge value in its error", () => {
  // Otherwise perfectly valid characters (no ".." , no bad chars) — this
  // must be rejected purely on length, and the error must still not echo
  // the whole 10kB string.
  const huge = "x".repeat(10_000);
  const result = validateToolArgs(byName("start_resolve_conflict"), { project: huge, repo: "sample-app", prId: 1 });
  assert.equal(result.ok, false);
  assert.ok(result.error.length < 500, "error message should be short, not echo the whole 10kB string");
  assert.ok(!result.error.includes(huge));
});

// ---- length bounds (must match what inputSchemaFor advertises) ----

test("validateToolArgs accepts a repoSegment of exactly 100 chars", () => {
  const project = `A${"b".repeat(99)}`;
  assert.equal(project.length, 100);
  const result = validateToolArgs(byName("start_resolve_conflict"), { project, repo: "sample-app", prId: 1 });
  assert.equal(result.ok, true);
  assert.equal(result.value.project, project);
});

test("validateToolArgs rejects a repoSegment of 101 chars, otherwise valid", () => {
  const project = `A${"b".repeat(100)}`;
  assert.equal(project.length, 101);
  const result = validateToolArgs(byName("start_resolve_conflict"), { project, repo: "sample-app", prId: 1 });
  assert.equal(result.ok, false);
  assert.match(result.error, /project/);
});

test("validateToolArgs accepts an issueKey of exactly 40 chars", () => {
  const issueKey = `A${"X".repeat(37)}-1`;
  assert.equal(issueKey.length, 40);
  const result = validateToolArgs(byName("get_issue_analysis"), { issueKey });
  assert.equal(result.ok, true);
  assert.equal(result.value.issueKey, issueKey.toUpperCase());
});

test("validateToolArgs rejects an issueKey of 41 chars, otherwise valid", () => {
  const issueKey = `A${"X".repeat(38)}-1`;
  assert.equal(issueKey.length, 41);
  const result = validateToolArgs(byName("get_issue_analysis"), { issueKey });
  assert.equal(result.ok, false);
  assert.match(result.error, /issueKey/);
});

// ---- selectTools ----

const ALL_FEATURES = registry.allFeatureIds();

test("selectTools includes generic readers, last, when no external servers are configured", () => {
  const tools = selectTools({ enabledFeatureIds: ALL_FEATURES, externalServers: [] });
  const generics = tools.filter((d) => d.generic);
  assert.deepEqual(generics.map((d) => d.name), ["jira_get_issue", "bitbucket_get_pull_request", "bitbucket_list_open_comments"]);
  const firstGenericIndex = tools.findIndex((d) => d.generic);
  for (let i = firstGenericIndex; i < tools.length; i++) {
    assert.equal(tools[i].generic, true);
  }
});

test("selectTools drops only the Jira reader when a Jira MCP server is configured", () => {
  const names = selectTools({ enabledFeatureIds: ALL_FEATURES, externalServers: ["acme-jira-confluence"] }).map((d) => d.name);
  assert.ok(!names.includes("jira_get_issue"));
  assert.ok(names.includes("bitbucket_get_pull_request"));
  assert.ok(names.includes("bitbucket_list_open_comments"));
});

test("selectTools drops only the Bitbucket readers when a Bitbucket MCP server is configured", () => {
  const names = selectTools({ enabledFeatureIds: ALL_FEATURES, externalServers: ["Acme-Bitbucket"] }).map((d) => d.name);
  assert.ok(names.includes("jira_get_issue"));
  assert.ok(!names.includes("bitbucket_get_pull_request"));
  assert.ok(!names.includes("bitbucket_list_open_comments"));
});

test("selectTools drops every generic reader when both a Jira and a Bitbucket server are configured", () => {
  const tools = selectTools({ enabledFeatureIds: ALL_FEATURES, externalServers: ["acme-jira-confluence", "acme-bitbucket", "wiki"] });
  assert.ok(tools.every((d) => !d.generic));
});

test("selectTools ignores unrelated external servers", () => {
  const tools = selectTools({ enabledFeatureIds: ALL_FEATURES, externalServers: ["wiki", "acme-jenkins-dii"] });
  assert.equal(tools.filter((d) => d.generic).length, 3);
});

test("selectTools disabling resolve-conflict drops only start_resolve_conflict", () => {
  const enabled = ALL_FEATURES.filter((id) => id !== "resolve-conflict");
  const tools = selectTools({ enabledFeatureIds: enabled, externalServers: ["jira", "bitbucket"] });
  const names = tools.map((d) => d.name);
  assert.ok(!names.includes("start_resolve_conflict"));
  assert.ok(names.includes("start_address_review_comments"));
});

test("selectTools with no features enabled still keeps the core tools", () => {
  const tools = selectTools({ enabledFeatureIds: [], externalServers: ["jira", "bitbucket"] });
  const names = tools.map((d) => d.name);
  assert.ok(names.includes("list_jobs"));
  assert.ok(names.includes("get_job"));
  assert.ok(names.includes("lookup_local_repo"));
  assert.ok(!names.includes("find_deployable_build"));
});


// ---- capToolOutput ----

test("capToolOutput passes a short value through as pretty JSON", () => {
  const result = capToolOutput({ a: 1 });
  assert.equal(result, JSON.stringify({ a: 1 }, null, 2));
});

test("capToolOutput truncates a huge value with a trailing note", () => {
  const big = { s: "x".repeat(200_000) };
  const result = capToolOutput(big, 1000);
  assert.equal(result.length > 1000, true);
  assert.ok(result.startsWith(JSON.stringify(big, null, 2).slice(0, 1000)));
  assert.match(result, /\n… \(truncated: \d+ more characters; ask for a narrower result\)$/);
});

// ---- PARAM_TYPES sanity ----

test("PARAM_TYPES covers every type used in the catalog", () => {
  const used = new Set();
  for (const def of TOOL_CATALOG) {
    for (const spec of Object.values(def.params)) used.add(spec.type);
  }
  for (const type of used) {
    assert.ok(type in PARAM_TYPES, `PARAM_TYPES missing ${type}`);
  }
});

// ---- history tools ----

test("history tools exist, and forget_item is the one local-write tool", () => {
  assert.equal(byName("get_history").kind, "read");
  assert.equal(byName("list_history").kind, "read");
  assert.deepEqual(TOOL_CATALOG.filter((d) => d.kind === "local-write").map((d) => d.name), ["forget_item"]);
});

test("history keys are normalized to the stored form, and junk is rejected", () => {
  assert.deepEqual(validateToolArgs(byName("get_history"), { key: "proj-12" }), { ok: true, value: { key: "jira:PROJ-12" } });
  assert.equal(validateToolArgs(byName("get_history"), { key: "bad\nkey" }).ok, false);
  assert.equal(validateToolArgs(byName("get_history"), {}).ok, false);
  assert.equal(validateToolArgs(byName("forget_item"), { key: "job:abc" }).ok, true);
});

test("list_history takes optional words and a window in days", () => {
  assert.equal(validateToolArgs(byName("list_history"), {}).ok, true);
  assert.deepEqual(validateToolArgs(byName("list_history"), { query: " login ", days: "7" }), {
    ok: true,
    value: { query: "login", days: 7 },
  });
  assert.equal(validateToolArgs(byName("list_history"), { query: "" }).ok, false);
  assert.equal(validateToolArgs(byName("list_history"), { days: 0 }).ok, false);
});

test("open_location is a core tool of kind local-action, and the only one", () => {
  assert.equal(byName("open_location").kind, "local-action");
  assert.equal(byName("open_location").featureId, null);
  assert.deepEqual(TOOL_CATALOG.filter((d) => d.kind === "local-action").map((d) => d.name), ["open_location"]);
});

test("open_location takes the pasted text (multi-line allowed, up to 8 KB) and nothing else", () => {
  assert.deepEqual(validateToolArgs(byName("open_location"), { text: "at f (src/a.ts:10:5)\n  at g (src/b.ts:2:1)" }), {
    ok: true,
    value: { text: "at f (src/a.ts:10:5)\n  at g (src/b.ts:2:1)" },
  });
  assert.equal(validateToolArgs(byName("open_location"), { text: "" }).ok, false);
  assert.equal(validateToolArgs(byName("open_location"), { text: "x".repeat(8193) }).ok, false);
  assert.equal(validateToolArgs(byName("open_location"), { text: "a\u0000b" }).ok, false);
  assert.equal(validateToolArgs(byName("open_location"), {}).ok, false);
  assert.equal(validateToolArgs(byName("open_location"), { text: "a.ts:1", extra: 1 }).ok, false);
});

test("ticket_workspace is a read tool offered only while ticket-workspace is enabled", () => {
  const def = byName("ticket_workspace");
  assert.equal(def.kind, "read");
  assert.equal(def.featureId, "ticket-workspace");
  assert.deepEqual(inputSchemaFor(def).required, ["issueKey"]);
  const on = selectTools({ enabledFeatureIds: ["ticket-workspace"], externalServers: ["jira", "bitbucket"] });
  assert.ok(on.map((d) => d.name).includes("ticket_workspace"));
  const off = selectTools({ enabledFeatureIds: [], externalServers: ["jira", "bitbucket"] });
  assert.ok(!off.map((d) => d.name).includes("ticket_workspace"));
  assert.deepEqual(validateToolArgs(def, { issueKey: "proj-7" }), { ok: true, value: { issueKey: "PROJ-7" } });
});
