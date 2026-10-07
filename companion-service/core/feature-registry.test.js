const test = require("node:test");
const assert = require("node:assert/strict");
const {
  FEATURE_DESCRIPTORS,
  allFeatureIds,
  defaultEnabledFeatureIds,
  enabledFeatureIds,
  requiredChecksFor,
  needsRepos,
  featuresRequiring,
} = require("./feature-registry.js");

test("every descriptor has the fields setup.js and server.ts depend on", () => {
  for (const d of FEATURE_DESCRIPTORS) {
    assert.equal(typeof d.id, "string");
    assert.ok(d.id.length > 0);
    assert.equal(typeof d.label, "string");
    assert.equal(typeof d.description, "string");
    assert.ok(Array.isArray(d.requiredChecks));
    // promptSetup is optional: a feature with nothing to ask has none.
    if ("promptSetup" in d) assert.equal(typeof d.promptSetup, "function");
    // enabledByDefault is optional; when present it must be a boolean.
    if ("enabledByDefault" in d) assert.equal(typeof d.enabledByDefault, "boolean");
  }
});

test("descriptor ids are unique", () => {
  const ids = FEATURE_DESCRIPTORS.map((d) => d.id);
  assert.equal(new Set(ids).size, ids.length);
});

test("allFeatureIds returns every descriptor id in declared order", () => {
  assert.deepEqual(allFeatureIds(), FEATURE_DESCRIPTORS.map((d) => d.id));
});

test("defaultEnabledFeatureIds excludes enabledByDefault:false descriptors", () => {
  assert.ok(allFeatureIds().includes("analyze-issue"));
  assert.ok(defaultEnabledFeatureIds().includes("analyze-issue"));
  assert.deepEqual(
    defaultEnabledFeatureIds(),
    FEATURE_DESCRIPTORS.filter((d) => d.enabledByDefault !== false).map((d) => d.id),
  );
});

test("analyze-issue is enabled by default", () => {
  const d = FEATURE_DESCRIPTORS.find((x) => x.id === "analyze-issue");
  assert.ok(d);
  assert.notEqual(d.enabledByDefault, false);
  assert.ok(defaultEnabledFeatureIds().includes("analyze-issue"));
});

test("enabledFeatureIds defaults to every known feature when the field is absent", () => {
  assert.deepEqual(enabledFeatureIds({}), allFeatureIds());
  assert.deepEqual(enabledFeatureIds({ port: 8787 }), allFeatureIds());
});

test("enabledFeatureIds soft-migrates a legacy list missing knownFeatures (enables newly shipped default-on ids)", () => {
  // Before knownFeatures existed, an explicit list froze the set forever —
  // soft-migrate treats ids absent from that list as newly shipped, but
  // only auto-enables ones that are enabledByDefault omit/true.
  assert.deepEqual(
    enabledFeatureIds({ enabledFeatures: ["resolve-conflict"] }),
    defaultEnabledFeatureIds(),
  );
  assert.deepEqual(enabledFeatureIds({ enabledFeatures: [] }), defaultEnabledFeatureIds());
});

test("enabledFeatureIds respects an intentional disable once knownFeatures is recorded", () => {
  assert.deepEqual(
    enabledFeatureIds({
      enabledFeatures: ["resolve-conflict"],
      knownFeatures: allFeatureIds(),
    }),
    ["resolve-conflict"],
  );
  assert.deepEqual(
    enabledFeatureIds({
      enabledFeatures: [],
      knownFeatures: allFeatureIds(),
    }),
    [],
  );
});

test("enabledFeatureIds drops ids that don't correspond to a known feature", () => {
  assert.deepEqual(
    enabledFeatureIds({
      enabledFeatures: ["resolve-conflict", "some-removed-feature"],
      knownFeatures: allFeatureIds(),
    }),
    ["resolve-conflict"],
  );
});

test("migrateEnabledFeatures returns null when already up to date", () => {
  const { migrateEnabledFeatures } = require("./feature-registry.js");
  const config = {
    enabledFeatures: ["resolve-conflict"],
    knownFeatures: allFeatureIds(),
  };
  assert.equal(migrateEnabledFeatures(config), null);
  assert.equal(migrateEnabledFeatures({}), null);
});

test("migrateEnabledFeatures enables default-on ids absent from a legacy list and records knownFeatures", () => {
  const { migrateEnabledFeatures } = require("./feature-registry.js");
  const result = migrateEnabledFeatures({ enabledFeatures: ["resolve-conflict"] });
  assert.deepEqual(result.enabledFeatures, defaultEnabledFeatureIds());
  assert.deepEqual(result.knownFeatures, allFeatureIds());
  assert.ok(result.enabledFeatures.includes("analyze-issue"));
});

test("migrateEnabledFeatures auto-enables analyze-issue when newly shipped", () => {
  const { migrateEnabledFeatures } = require("./feature-registry.js");
  // Pretend knownFeatures was written before analyze-issue existed.
  const prior = allFeatureIds().filter((id) => id !== "analyze-issue");
  const result = migrateEnabledFeatures({
    enabledFeatures: ["resolve-conflict"],
    knownFeatures: prior,
  });
  assert.ok(result.enabledFeatures.includes("resolve-conflict"));
  assert.ok(result.enabledFeatures.includes("analyze-issue"));
  assert.equal(result.enabledFeatures.includes("create-jira-subtasks"), false);
  assert.ok(result.knownFeatures.includes("analyze-issue"));
  assert.deepEqual(result.knownFeatures, allFeatureIds());
});


test("requiredChecksFor unions checks across exactly the given ids, deduplicated", () => {
  assert.deepEqual(requiredChecksFor(["resolve-conflict"]).sort(), ["claudeAuth", "claudeCli", "git"]);
  assert.deepEqual(requiredChecksFor(["create-jira-subtasks"]), []);
  assert.deepEqual(requiredChecksFor(["review-in-editor"]), ["git"]);
  assert.deepEqual(requiredChecksFor([]), []);
});

test("requiredChecksFor ignores an id that isn't a real descriptor", () => {
  assert.deepEqual(requiredChecksFor(["not-a-real-feature"]), []);
});

test("needsRepos is true only when a selected feature works on local clones", () => {
  assert.equal(needsRepos(["resolve-conflict"]), true);
  assert.equal(needsRepos(["create-jira-subtasks", "review-in-editor"]), true);
  assert.equal(needsRepos(["create-jira-subtasks", "pre-deployment-stats"]), false);
  assert.equal(needsRepos([]), false);
});

test("featuresRequiring lists exactly the selected features that need a check", () => {
  const ids = ["resolve-conflict", "review-in-editor", "create-jira-subtasks"];
  assert.deepEqual(featuresRequiring("git", ids).map((d) => d.id), ["resolve-conflict", "review-in-editor"]);
  assert.deepEqual(featuresRequiring("claudeAuth", ids).map((d) => d.id), ["resolve-conflict"]);
  assert.deepEqual(featuresRequiring("git", ["create-jira-subtasks"]), []);
});

test("address-review-comments: exact descriptor fields, on by default, needs repos and Claude", () => {
  const d = FEATURE_DESCRIPTORS.find((x) => x.id === "address-review-comments");
  assert.ok(d);
  assert.equal(d.label, "Address review comments");
  assert.deepEqual(d.requiredChecks, ["git", "claudeCli", "claudeAuth"]);
  assert.equal(d.needsRepos, true);
  assert.notEqual(d.enabledByDefault, false);
  assert.ok(allFeatureIds().includes("address-review-comments"));
  assert.equal(defaultEnabledFeatureIds().includes("address-review-comments"), true);
});

test("address-review-comments and ticket-to-pr are auto-enabled when they newly ship to an existing install", () => {
  const { migrateEnabledFeatures } = require("./feature-registry.js");
  const prior = allFeatureIds().filter((id) => id !== "address-review-comments" && id !== "ticket-to-pr");
  const result = migrateEnabledFeatures({ enabledFeatures: ["resolve-conflict"], knownFeatures: prior });
  assert.ok(result.enabledFeatures.includes("address-review-comments"));
  assert.ok(result.enabledFeatures.includes("ticket-to-pr"));
  assert.ok(result.knownFeatures.includes("address-review-comments"));
});


function recordingHelpers(externalToken) {
  const asked = [];
  const logs = [];
  return {
    asked,
    logs,
    log: (m) => logs.push(m),
    async ask(_rl, question, def) {
      asked.push(question);
      return def;
    },
    externalToken,
  };
}
