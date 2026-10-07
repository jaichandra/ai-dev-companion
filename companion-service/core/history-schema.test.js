const test = require("node:test");
const assert = require("node:assert/strict");
const schema = require("./history-schema.js");

test("normalizeKey maps bare issue keys and PR refs to the scopeKey forms", () => {
  assert.equal(schema.normalizeKey("proj-12"), "jira:PROJ-12");
  assert.equal(schema.normalizeKey(" PROJ-12 "), "jira:PROJ-12");
  assert.equal(schema.normalizeKey("ci/Sample-App#7"), "bitbucket:CI/sample-app#7");
  assert.equal(schema.normalizeKey("jira:PROJ-12"), "jira:PROJ-12");
  assert.equal(schema.normalizeKey("job:abc"), "job:abc");
});

test("normalizeKey rejects empty, control-character and oversized keys", () => {
  assert.equal(schema.normalizeKey(""), null);
  assert.equal(schema.normalizeKey("a\nb"), null);
  assert.equal(schema.normalizeKey("x".repeat(301)), null);
  assert.equal(schema.normalizeKey(42), null);
});

test("clipExcerpt caps at 2 KB, strips control characters, and drops blanks", () => {
  assert.equal(schema.clipExcerpt("a".repeat(3000)).length, 2048);
  assert.equal(schema.clipExcerpt("hi\u0000 there\u0007"), "hi there");
  assert.equal(schema.clipExcerpt("keeps\nnewlines\tand tabs"), "keeps\nnewlines\tand tabs");
  assert.equal(schema.clipExcerpt("   "), null);
  assert.equal(schema.clipExcerpt(undefined), null);
});

test("summarizeEvents groups by feature and outcome with median and average", () => {
  const rows = [
    { feature_id: "resolve-conflict", outcome: "approved", duration_ms: 100 },
    { feature_id: "resolve-conflict", outcome: "approved", duration_ms: 300 },
    { feature_id: "resolve-conflict", outcome: "approved", duration_ms: 200 },
    { feature_id: "resolve-conflict", outcome: "discarded", duration_ms: null },
    { feature_id: "analyze-issue", outcome: "completed", duration_ms: 50 },
  ];
  assert.deepEqual(schema.summarizeEvents(rows), [
    { featureId: "analyze-issue", outcome: "completed", count: 1, medianMs: 50, avgMs: 50 },
    { featureId: "resolve-conflict", outcome: "approved", count: 3, medianMs: 200, avgMs: 200 },
    { featureId: "resolve-conflict", outcome: "discarded", count: 1, medianMs: null, avgMs: null },
  ]);
});
