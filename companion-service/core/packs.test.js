// core/packs.js: validation, merging, and what the loaded pack declares (against
// the build — npm run build first — because a feature's factory loads its compiled module).
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const packs = require("../dist/core/packs.js");
const environment = require("../dist/environment.js");

const feature = (id, extra = {}) => ({ descriptor: { id, label: id }, factory: () => () => ({ id }), ...extra });

// ---- definePack / mergePacks ----

test("definePack accepts a well-formed pack and returns it", () => {
  const pack = { id: "p", features: [feature("one"), feature("two", { scopeKey: "jira-issue", persist: false })] };
  assert.equal(packs.definePack(pack), pack);
});

test("definePack names the pack and feature in every complaint", () => {
  assert.throws(() => packs.definePack({ features: [] }), /needs an id/);
  assert.throws(() => packs.definePack({ id: "p" }), /features must be an array/);
  assert.throws(() => packs.definePack({ id: "p", features: [{ descriptor: { label: "x" }, factory() {} }] }), /needs descriptor\.id/);
  assert.throws(() => packs.definePack({ id: "p", features: [{ descriptor: { id: "Bad_Id", label: "x" } }] }), /needs descriptor\.id/);
  assert.throws(() => packs.definePack({ id: "p", features: [{ descriptor: { id: "a" }, factory() {} }] }), /p\/a.*label is required/);
  assert.throws(() => packs.definePack({ id: "p", features: [{ descriptor: { id: "a", label: "A" }, factory: "nope" }] }), /p\/a.*factory must be a function/);
  assert.throws(() => packs.definePack({ id: "p", features: [feature("a", { scopeKey: "nope" })] }), /p\/a.*scopeKey must be/);
  assert.throws(() => packs.definePack({ id: "p", features: [feature("a", { persist: "no" })] }), /p\/a.*persist must be a boolean/);
});

test("definePack refuses a feature listed twice and a tool that names another feature", () => {
  assert.throws(() => packs.definePack({ id: "p", features: [feature("a"), feature("a")] }), /"a" is listed twice/);
  const stray = feature("a", { mcpTools: [{ name: "t", featureId: "b" }] });
  assert.throws(() => packs.definePack({ id: "p", features: [stray] }), /MCP tool t must name this feature/);
});

test("mergePacks refuses a feature id two packs both provide", () => {
  const one = packs.definePack({ id: "one", features: [feature("shared")] });
  const two = packs.definePack({ id: "two", features: [feature("shared")] });
  assert.throws(() => packs.mergePacks([one, two]), /feature "shared" is already provided by pack "one"/);
  assert.deepEqual(packs.mergePacks([one]), [one]);
});

// ---- the loaded pack ----


test("every feature's factory loads a create function from its compiled module", () => {
  const factories = packs.featureFactories();
  for (const spec of packs.features()) {
    const create = spec.factory();
    assert.equal(typeof create, "function", spec.descriptor.id);
    assert.equal(typeof factories[spec.descriptor.id], "function", spec.descriptor.id);
  }
});





test("every pack feature names the extension script that registers it, and the script exists", () => {
  const root = path.join(__dirname, "..", "..", "chrome-extension");
  for (const spec of packs.features()) {
    assert.ok(spec.extension, `${spec.descriptor.id} has no extension script`);
    assert.ok(fs.existsSync(path.join(root, spec.extension.script)), `${spec.extension.script} is missing`);
  }
});

test("definePack checks a feature's extension script", () => {
  const withExt = (extension) => ({ id: "p", features: [feature("a", { extension })] });
  assert.doesNotThrow(() => packs.definePack(withExt({ script: "features/a.js", order: 5 })));
  for (const bad of [{ script: "../a.js" }, { script: "/etc/a.js" }, { script: "a.txt" }, { script: "features/../a.js" }, {}, { script: "a.js", order: "1" }]) {
    assert.throws(() => packs.definePack(withExt(bad)), /extension\./, JSON.stringify(bad));
  }
});

// ---- pack-level hooks ----

test("definePack checks the settings and targets hooks a pack contributes", () => {
  const ok = { validate: () => ({ ok: true, value: 1 }), from: () => 1, defaultValue: 1 };
  assert.doesNotThrow(() => packs.definePack({ id: "p", features: [], siteSettings: { jenkins: { k: ok } }, targets: () => ({}) }));
  assert.throws(() => packs.definePack({ id: "p", features: [], targets: {} }), /targets must be a function/);
  assert.throws(
    () => packs.definePack({ id: "p", features: [], siteSettings: { jenkins: { k: { validate() {}, from() {} } } } }),
    /siteSettings\.jenkins\.k needs validate\(\), from\(\) and defaultValue/,
  );
});



test("definePack checks the externalTokens, diagnose, doctorChecks and mcpDenyList hooks", () => {
  const fn = () => {};
  assert.doesNotThrow(() =>
    packs.definePack({
      id: "p",
      features: [],
      externalTokens: { label: "x", sources: [fn] },
      diagnose: { available: fn, args: fn, summary: fn },
      doctorChecks: [fn],
      mcpDenyList: ["mcp__x__y"],
      analysisServerHints: [{ server: "wiki", whenAvailable: "a", whenMissing: "b" }],
    }),
  );
  assert.throws(() => packs.definePack({ id: "p", features: [], externalTokens: { label: "x" } }), /externalTokens needs a label/);
  assert.throws(() => packs.definePack({ id: "p", features: [], diagnose: { available: fn } }), /diagnose needs available\(\), args\(\) and summary\(\)/);
  assert.throws(() => packs.definePack({ id: "p", features: [], doctorChecks: [1] }), /doctorChecks must be a list of functions/);
  assert.throws(() => packs.definePack({ id: "p", features: [], mcpDenyList: [""] }), /mcpDenyList must be a list of tool names/);
  assert.throws(() => packs.definePack({ id: "p", features: [], analysisServerHints: [{ server: "wiki" }] }), /analysisServerHints must be a list/);
});

test("scope keys follow each builtin feature's declared rule", () => {
  const kinds = { pr: () => "pr-key", "jira-issue": () => "jira-key" };
  assert.equal(packs.scopeKeyFor("resolve-conflict", {}, kinds), "pr-key");
  assert.equal(packs.scopeKeyFor("address-review-comments", {}, kinds), "pr-key");
  assert.equal(packs.scopeKeyFor("ticket-to-pr", {}, kinds), "jira-key");
  assert.equal(packs.scopeKeyFor("digest", {}, kinds), null, "no rule, no key");
  assert.equal(packs.scopeKeyFor("not-a-feature", {}, kinds), null);
});

test("a feature may name the pull-request key kind as 'pr' (or its old name 'bitbucket-pr')", () => {
  for (const scopeKey of ["pr", "bitbucket-pr", "jira-issue"]) {
    assert.doesNotThrow(() => packs.definePack({ id: "p", features: [feature("a", { scopeKey })] }), scopeKey);
  }
  assert.throws(() => packs.definePack({ id: "p", features: [feature("a", { scopeKey: "github-pr" })] }), /scopeKey must be/);
});
