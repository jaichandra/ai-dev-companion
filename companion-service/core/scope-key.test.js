const test = require("node:test");
const assert = require("node:assert/strict");
const { scopeKeyFor, isValidScopeKey, lookupJob } = require("./scope-key.js");

// ---- scopeKeyFor ----

test("scopeKeyFor: resolve-conflict builds a bitbucket key, uppercasing the project and lowercasing the repo", () => {
  assert.equal(scopeKeyFor("resolve-conflict", { project: "ci", repo: "Sample-App", prId: 12 }), "bitbucket:CI/sample-app#12");
});

test("scopeKeyFor: address-review-comments builds the same bitbucket key shape", () => {
  assert.equal(
    scopeKeyFor("address-review-comments", { project: "ACME", repo: "sample-app", prId: 7 }),
    "bitbucket:ACME/sample-app#7",
  );
});

test("scopeKeyFor: analyze-issue builds a jira key, uppercasing the issue key", () => {
  assert.equal(scopeKeyFor("analyze-issue", { issueKey: "proj-1" }), "jira:PROJ-1");
});

test("scopeKeyFor: analyze-issue rejects an issue key with no ticket number", () => {
  assert.equal(scopeKeyFor("analyze-issue", { issueKey: "PROJ" }), null);
});


test("isValidScopeKey accepts jenkins:<helm job> for any safe job name and nothing malformed", () => {
  assert.equal(isValidScopeKey("jenkins:helm_other"), true);
  assert.equal(isValidScopeKey("jenkins:bad job"), false);
  assert.equal(isValidScopeKey("jenkins:"), false);
});

test("scopeKeyFor: an unknown feature id gives null", () => {
  assert.equal(scopeKeyFor("some-other-feature", { project: "CI", repo: "sample-app", prId: 12 }), null);
});

test("scopeKeyFor: a prId given as a digit string works", () => {
  assert.equal(scopeKeyFor("resolve-conflict", { project: "CI", repo: "sample-app", prId: "12" }), "bitbucket:CI/sample-app#12");
});

for (const badPrId of [0, -1, "12a", 1.5]) {
  test(`scopeKeyFor: a prId of ${JSON.stringify(badPrId)} gives null`, () => {
    assert.equal(scopeKeyFor("resolve-conflict", { project: "CI", repo: "sample-app", prId: badPrId }), null);
  });
}

test("scopeKeyFor: a project containing a slash gives null", () => {
  assert.equal(scopeKeyFor("resolve-conflict", { project: "CI/x", repo: "sample-app", prId: 12 }), null);
});

test("scopeKeyFor: a project containing '..' gives null", () => {
  assert.equal(scopeKeyFor("resolve-conflict", { project: "..", repo: "sample-app", prId: 12 }), null);
});

test("scopeKeyFor: a malformed payload gives null", () => {
  assert.equal(scopeKeyFor("resolve-conflict", null), null);
  assert.equal(scopeKeyFor("resolve-conflict", {}), null);
  assert.equal(scopeKeyFor("analyze-issue", {}), null);
});

// ---- isValidScopeKey ----

test("isValidScopeKey accepts exactly the shapes scopeKeyFor produces", () => {
  assert.equal(isValidScopeKey("bitbucket:CI/sample-app#12"), true);
  assert.equal(isValidScopeKey("jira:PROJ-1"), true);
  assert.equal(isValidScopeKey("jenkins:helm_main"), true);
});

test("isValidScopeKey accepts the Phase 5 per-build jenkins shape", () => {
  assert.equal(isValidScopeKey("jenkins:helm_main#42"), true);
});

test("isValidScopeKey rejects a key with a trailing newline", () => {
  assert.equal(isValidScopeKey("bitbucket:CI/sample-app#12\n"), false);
});

test("isValidScopeKey rejects an injection attempt in place of the build number", () => {
  assert.equal(isValidScopeKey("jira:PROJ-1 OR 1=1"), false);
});

test("isValidScopeKey rejects a key over 300 characters", () => {
  assert.equal(isValidScopeKey(`bitbucket:CI/sample-app#${"1".repeat(301)}`), false);
});

test("isValidScopeKey rejects a jenkins key whose job name isn't a safe segment", () => {
  assert.equal(isValidScopeKey("jenkins:other job"), false);
  assert.equal(isValidScopeKey("jenkins:../other_job"), false);
});

test("isValidScopeKey rejects non-string input", () => {
  assert.equal(isValidScopeKey(undefined), false);
  assert.equal(isValidScopeKey(null), false);
});

// ---- lookupJob ----

function job(overrides) {
  return {
    id: "id",
    featureId: "resolve-conflict",
    status: "running",
    data: {},
    scopeKey: "bitbucket:CI/sample-app#12",
    startedVia: "mcp",
    createdAt: 0,
    updatedAt: 0,
    ...overrides,
  };
}

test("lookupJob picks the newest matching mcp job", () => {
  const older = job({ id: "older", createdAt: 1 });
  const newer = job({ id: "newer", createdAt: 2 });
  const result = lookupJob([older, newer], "bitbucket:CI/sample-app#12", "resolve-conflict");
  assert.equal(result.id, "newer");
});

test("lookupJob ignores jobs started from the extension", () => {
  const extensionJob = job({ id: "extension", startedVia: "extension", createdAt: 5 });
  const mcpJob = job({ id: "mcp", startedVia: "mcp", createdAt: 1 });
  const result = lookupJob([extensionJob, mcpJob], "bitbucket:CI/sample-app#12", "resolve-conflict");
  assert.equal(result.id, "mcp");
});

test("lookupJob ignores approved and rejected jobs", () => {
  const approved = job({ id: "approved", status: "approved", createdAt: 5 });
  const rejected = job({ id: "rejected", status: "rejected", createdAt: 4 });
  const pending = job({ id: "pending", status: "pending-start", createdAt: 1 });
  const result = lookupJob([approved, rejected, pending], "bitbucket:CI/sample-app#12", "resolve-conflict");
  assert.equal(result.id, "pending");
});

test("lookupJob ignores jobs for a different feature on the same scopeKey", () => {
  const otherFeature = job({ id: "other", featureId: "address-review-comments", createdAt: 5 });
  const thisFeature = job({ id: "this", featureId: "resolve-conflict", createdAt: 1 });
  const result = lookupJob([otherFeature, thisFeature], "bitbucket:CI/sample-app#12", "resolve-conflict");
  assert.equal(result.id, "this");
});

test("lookupJob returns null when nothing matches", () => {
  const result = lookupJob([job({ id: "a" })], "jira:PROJ-1", "analyze-issue");
  assert.equal(result, null);
});

test("scopeKeyFor: the ticket features scope by the Jira issue, like analyze-issue", () => {
  assert.equal(scopeKeyFor("ticket-workspace", { issueKey: "proj-7" }), "jira:PROJ-7");
  assert.equal(scopeKeyFor("ticket-to-pr", { issueKey: "PROJ-7", adopt: true }), "jira:PROJ-7");
  assert.equal(scopeKeyFor("ticket-to-pr", { issueKey: "PROJ" }), null);
});

test("a GitHub PR's key is accepted, with the same safety checks as Bitbucket's", () => {
  assert.equal(isValidScopeKey("github:octo/hello#42"), true);
  assert.equal(isValidScopeKey("github:octo/.github#1"), true);
  assert.equal(isValidScopeKey("github:octo/hello#0"), false);
  assert.equal(isValidScopeKey("github:octo/hello#4x"), false);
  assert.equal(isValidScopeKey("github:../x/hello#1"), false);
  assert.equal(isValidScopeKey("github:octo/hello#1\n"), false);
  assert.equal(isValidScopeKey("gitlab:octo/hello#1"), false);
});

test("a pull request feature's scope key follows the profile's git host, and 'bitbucket-pr' still means the same", () => {
  assert.equal(scopeKeyFor("resolve-conflict", { project: "acme", repo: "Sample-App", prId: 7 }), "bitbucket:ACME/sample-app#7");
  assert.equal(scopeKeyFor("address-review-comments", { project: "acme", repo: "Sample-App", prId: "7" }), "bitbucket:ACME/sample-app#7");
  assert.equal(scopeKeyFor("resolve-conflict", { project: "../x", repo: "r", prId: 7 }), null);
  assert.equal(scopeKeyFor("resolve-conflict", { project: "o", repo: "r", prId: 0 }), null);
});
