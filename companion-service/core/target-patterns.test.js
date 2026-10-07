const test = require("node:test");
const assert = require("node:assert/strict");
const P = require("../../chrome-extension/target-patterns.js");

test("the issue pattern follows the configured projects", () => {
  const re = P.issueUrlPattern({ pipelines: [], analyzeIssue: { projects: ["PROJ", "CI"], issueTypes: ["Bug"] } });
  assert.equal("https://jira.example.com/browse/PROJ-123".match(re)[1], "PROJ-123");
  assert.equal("https://jira.example.com/browse/CI-7?focusedId=1".match(re)[1], "CI-7");
  assert.equal("https://jira.example.com/browse/OTHER-1".match(re), null);
  assert.equal("https://jira.example.com/browse/PROJ".match(re), null);
  // Until /targets answers there are no configured projects, so any project key matches.
  assert.equal("https://jira.example.com/browse/PROJ-123".match(P.issueUrlPattern(P.DEFAULT_TARGETS))[1], "PROJ-123");
  assert.equal("https://jira.example.com/browse/ABC_D-45".match(P.issueUrlPattern(P.DEFAULT_TARGETS))[1], "ABC_D-45");
  assert.equal("https://jira.example.com/browse/lower-1".match(P.issueUrlPattern(P.DEFAULT_TARGETS)), null);
});

test("validTargets accepts the defaults and rejects malformed answers", () => {
  assert.equal(P.validTargets(P.DEFAULT_TARGETS), true);
  for (const bad of [null, {}, { analyzeIssue: {} }, { analyzeIssue: { projects: "PROJ", issueTypes: [] } }, { analyzeIssue: { projects: [] } }, "x"]) {
    assert.equal(P.validTargets(bad), false, JSON.stringify(bad));
  }
});

test("targets with no issue types are valid (any type), but the lists must exist", () => {
  const ok = { analyzeIssue: { projects: ["PROJ"], issueTypes: [] } };
  assert.equal(P.validTargets(ok), true);
  assert.equal(P.validTargets({ ...ok, analyzeIssue: { projects: ["PROJ"] } }), false);
  assert.deepEqual(P.DEFAULT_TARGETS.analyzeIssue.issueTypes, []);
});

test("a pack's own targets (the Acme pipelines) don't make targets valid or invalid", () => {
  const ok = { analyzeIssue: { projects: ["PROJ"], issueTypes: [] } };
  assert.equal(P.validTargets({ ...ok, pipelines: [{ helmJob: "h" }] }), true);
  assert.equal(P.validTargets({ ...ok, pipelines: "anything" }), true);
  assert.equal(P.DEFAULT_TARGETS.pipelines, undefined);
});
