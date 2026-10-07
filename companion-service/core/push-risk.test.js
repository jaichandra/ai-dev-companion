const test = require("node:test");
const assert = require("node:assert/strict");
const { validateFacts } = require("./risk-facts.js");
const { normalizeChangedFiles, assessPushRisk } = require("./push-risk.js");

const NOW = Date.parse("2026-09-29T09:00:00Z");
const facts = validateFacts({
  schema: "risk-facts/v1",
  generatedAt: "2026-09-29T02:00:00Z",
  tests: [
    { name: "Login with SSO", flaky: true, failed: 8, of: 30 },
    { name: "Billing totals", flaky: false, failed: 1, of: 30 },
  ],
  files: [
    { path: "src/app/login.ts", regressions: [{ build: 812, tests: ["Login with SSO"] }, { build: 815, tests: ["Login with SSO"] }] },
    { path: "src/app/billing.ts", regressions: [{ build: 801, tests: ["Billing totals"] }, { build: 806, tests: [] }, { build: 809, tests: [] }] },
    { path: "src/app/quiet.ts", regressions: [] },
  ],
}).facts;

test("normalizeChangedFiles keeps repo-relative paths only", () => {
  assert.deepEqual(normalizeChangedFiles(["./a.ts", "a.ts", " b/c.ts "]), { ok: true, value: ["a.ts", "b/c.ts"] });
  for (const bad of ["/etc/passwd", "../x", "a/../../x", "a\nb", ""]) {
    assert.equal(normalizeChangedFiles([bad]).ok, false, JSON.stringify(bad));
  }
  assert.equal(normalizeChangedFiles("a.ts").ok, false);
  assert.equal(normalizeChangedFiles(new Array(501).fill("a")).ok, false);
});

test("no changed file in the facts: not enough data, never a guess", () => {
  const r = assessPushRisk({ files: ["README.md", "docs/x.md"], facts, now: NOW });
  assert.equal(r.level, null);
  assert.equal(r.summary, "Not enough data: none of the 2 changed files are in the shared test history.");
  assert.deepEqual(r.lines, []);
});

test("known files with no regressions: low", () => {
  const r = assessPushRisk({ files: ["src/app/quiet.ts", "README.md"], facts, now: NOW });
  assert.equal(r.level, "low");
  assert.match(r.summary, /^Low risk: 1 of 2 changed files is in the shared test history/);
});

test("one file in 1-2 regressions: medium, citing builds and tests, and the flaky test", () => {
  const r = assessPushRisk({ files: ["src/app/login.ts"], facts, now: NOW });
  assert.equal(r.level, "medium");
  assert.equal(r.summary, "Medium risk: 1 of 1 changed file was in earlier regressions.");
  assert.deepEqual(r.lines.map((l) => l.text), [
    "src/app/login.ts was in 2 regressions: 2 builds 812, 815 (tests: Login with SSO)",
    '"Login with SSO" is known flaky (failed 8 of 30 builds), so a failure there may not be yours',
  ]);
  assert.deepEqual(r.lines[0].evidence, { kind: "file", file: "src/app/login.ts", builds: [812, 815], tests: ["Login with SSO"] });
});

test("a file in 3+ regressions, or 2+ risky files: high; old facts are flagged", () => {
  assert.equal(assessPushRisk({ files: ["src/app/billing.ts"], facts, now: NOW }).level, "high");
  const both = assessPushRisk({ files: ["src/app/login.ts", "src/app/billing.ts"], facts, now: NOW });
  assert.equal(both.level, "high");
  assert.equal(both.lines[0].evidence.file, "src/app/billing.ts", "most regressions first");
  assert.equal(both.withRegressions, 2);
  const late = assessPushRisk({ files: ["src/app/login.ts"], facts, now: NOW + 8 * 24 * 3600 * 1000 });
  assert.match(late.lines.at(-1).text, /from 2026-09-29 and may be out of date/);
});

const many = (n, over = {}) => ({
  schema: "risk-facts/v1",
  generatedAt: "2026-09-29T02:00:00Z",
  tests: over.tests || [],
  files: Array.from({ length: n }, (_, i) => ({ path: `src/f${i}.ts`, regressions: [{ build: 100 + i, tests: over.testNames || [] }] })),
});

test("two risky files, each with one regression, are already high (isolated from the 3+ rule)", () => {
  const f = validateFacts(many(2)).facts;
  const r = assessPushRisk({ files: ["src/f0.ts", "src/f1.ts"], facts: f, now: NOW });
  assert.equal(r.level, "high");
  assert.equal(r.withRegressions, 2);
  assert.equal(assessPushRisk({ files: ["src/f0.ts"], facts: f, now: NOW }).level, "medium", "one such file alone is medium");
});

test("more than 5 risky files: 5 lines, then a 'more' line with the rest counted", () => {
  const f = validateFacts(many(8)).facts;
  const r = assessPushRisk({ files: Array.from({ length: 8 }, (_, i) => `src/f${i}.ts`), facts: f, now: NOW });
  assert.equal(r.lines.filter((l) => l.evidence.kind === "file").length, 5);
  const more = r.lines.find((l) => l.evidence.kind === "more");
  assert.equal(more.text, "…and 3 more files with regressions");
});

test("flaky lines are capped at 5", () => {
  const names = Array.from({ length: 8 }, (_, i) => `Flaky ${i}`);
  const f = validateFacts(many(1, { testNames: names, tests: names.map((name) => ({ name, flaky: true, failed: 3, of: 10 })) })).facts;
  const r = assessPushRisk({ files: ["src/f0.ts"], facts: f, now: NOW });
  assert.equal(r.lines.filter((l) => l.evidence.kind === "flaky").length, 5);
});

test("facts without a generatedAt are never called stale, and report null", () => {
  const f = validateFacts(many(1)).facts;
  f.generatedAt = undefined;
  const r = assessPushRisk({ files: ["src/f0.ts"], facts: f, now: NOW + 400 * 24 * 3600 * 1000 });
  assert.equal(r.generatedAt, null);
  assert.equal(r.lines.some((l) => l.evidence.kind === "stale"), false);
  f.generatedAt = "not a date";
  assert.equal(assessPushRisk({ files: ["src/f0.ts"], facts: f, now: NOW }).lines.some((l) => l.evidence.kind === "stale"), false);
});
