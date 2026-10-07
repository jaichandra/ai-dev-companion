const test = require("node:test");
const assert = require("node:assert/strict");
const targets = require("./targets.js");


test("validateProjects upper-cases and de-duplicates project keys, and rejects junk", () => {
  assert.deepEqual(targets.validateProjects(["proj", "CI", "PROJ"]), { ok: true, value: ["PROJ", "CI"] });
  for (const bad of [[], "PROJ", ["PROJ-1"], ["1PROJ"], ["a".repeat(21)], Array.from({ length: 21 }, (_, i) => `P${i}`)]) {
    assert.equal(targets.validateProjects(bad).ok, false, JSON.stringify(bad));
  }
});

test("validateIssueTypes trims and de-duplicates type names, and rejects junk", () => {
  assert.deepEqual(targets.validateIssueTypes([" Bug ", "Task", "Bug"]), { ok: true, value: ["Bug", "Task"] });
  assert.deepEqual(targets.validateIssueTypes([]), { ok: true, value: [] }); // empty = any type
  for (const bad of ["Bug", [""], ["a\nb"], ["x".repeat(41)]]) {
    assert.equal(targets.validateIssueTypes(bad).ok, false, JSON.stringify(bad));
  }
});

test("analyzeTargetsFrom reads valid lists and describeAnalyzeScope names them", () => {
  const t = targets.analyzeTargetsFrom({ analyzeIssue: { projects: ["ci", "proj"], issueTypes: ["Bug", "Task"] } });
  assert.deepEqual(t, { projects: ["CI", "PROJ"], issueTypes: ["Bug", "Task"] });
  assert.equal(targets.describeAnalyzeScope(t), "CI/PROJ Bugs/Tasks");
});

test("validateComponentRepoMap accepts component -> PROJECT/repo and rejects the rest", () => {
  assert.deepEqual(targets.validateComponentRepoMap({ UI: "ACME/sample-app", " API ": "ACME/sample-service" }), {
    ok: true,
    value: { UI: "ACME/sample-app", API: "ACME/sample-service" },
  });
  assert.deepEqual(targets.validateComponentRepoMap({}), { ok: true, value: {} });
  for (const bad of [[], "x", null, { UI: "sample-app" }, { UI: "ACME/sample-app/extra" }, { UI: "ACME/../x" }, { "": "ACME/sample-app" }, { UI: 5 }, { "a\nb": "ACME/sample-app" }]) {
    assert.equal(targets.validateComponentRepoMap(bad).ok, false, JSON.stringify(bad));
  }
});

test("an empty issue-type list means any type, and a list still narrows it", () => {
  assert.deepEqual(targets.analyzeTargetsFrom({ analyzeIssue: { issueTypes: [] } }).issueTypes, []);
  assert.deepEqual(targets.analyzeTargetsFrom({ analyzeIssue: { issueTypes: ["Customer Issue"] } }).issueTypes, ["Customer Issue"]);
  assert.equal(targets.describeAnalyzeScope({ projects: ["ICI"], issueTypes: [] }), "ICI tickets");
});
