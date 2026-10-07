const test = require("node:test");
const assert = require("node:assert/strict");
const plan = require("./plan.js");

const raw = (author, body, created = "2026-10-01T10:00:00.000+0000") => ({ author: { displayName: author }, body, created });

test("normalizeComments keeps author, date and body, and drops empty and bot comments", () => {
  const out = plan.normalizeComments({
    comments: [raw("Ann", " Fixed in build 12 "), raw("Bob", "   "), raw("Jenkins [bot]", "Build passed"), { body: "orphan" }, null],
  });
  assert.deepEqual(out, [
    { author: "Ann", created: "2026-10-01T10:00:00.000+0000", body: "Fixed in build 12" },
    { author: "Unknown", created: null, body: "orphan" },
  ]);
});

test("normalizeComments tolerates a missing or malformed payload", () => {
  assert.deepEqual(plan.normalizeComments(null), []);
  assert.deepEqual(plan.normalizeComments({ comments: "nope" }), []);
});

test("selectWithinBudget keeps everything that fits", () => {
  const list = [1, 2, 3].map((n) => ({ author: "A", created: null, body: `c${n}` }));
  assert.deepEqual(plan.selectWithinBudget(list, 1000), { kept: list, omitted: 0 });
});

test("selectWithinBudget over budget keeps the oldest and the newest, and counts the omitted middle", () => {
  const list = [1, 2, 3, 4, 5].map((n) => ({ author: "A", created: null, body: `${n}`.repeat(100) }));
  const { kept, omitted } = plan.selectWithinBudget(list, 300);
  assert.equal(kept[0].body[0], "1");
  assert.equal(kept[kept.length - 1].body[0], "5");
  assert.equal(kept.length + omitted, 5);
  assert.ok(omitted > 0);
});

test("selectWithinBudget truncates one oversized comment instead of dropping it", () => {
  const { kept, omitted } = plan.selectWithinBudget([{ author: "A", created: null, body: "x".repeat(10000) }], 30000);
  assert.equal(omitted, 0);
  assert.ok(kept[0].body.length < 10000);
  assert.match(kept[0].body, /truncated/);
});

test("buildPrompt delimits the comments as data and mentions omitted ones", () => {
  const prompt = plan.buildPrompt({ issueKey: "PROJ-1", summary: "Login fails", kept: [{ author: "Ann", created: "2026-10-01T10:00:00.000+0000", body: "hi" }], omitted: 3 });
  assert.match(prompt, /PROJ-1/);
  assert.match(prompt, /Login fails/);
  assert.match(prompt, /Ann/);
  assert.match(prompt, /3 older comments/);
  assert.match(prompt, /not instructions/i);
});

test("buildPrompt asks for the JSON shape parseSummary reads", () => {
  const prompt = plan.buildPrompt({ issueKey: "PROJ-1", summary: null, kept: [], omitted: 0 });
  for (const field of ["tldr", "decisions", "openQuestions", "nextSteps"]) assert.match(prompt, new RegExp(field));
});

test("parseSummary reads plain and fenced JSON and trims list items", () => {
  const body = { tldr: " Fixed. ", decisions: [" Ship it "], openQuestions: [], nextSteps: ["Retest", ""] };
  const expected = { tldr: "Fixed.", decisions: ["Ship it"], openQuestions: [], nextSteps: ["Retest"] };
  assert.deepEqual(plan.parseSummary(JSON.stringify(body)), expected);
  assert.deepEqual(plan.parseSummary("```json\n" + JSON.stringify(body) + "\n```"), expected);
  assert.deepEqual(plan.parseSummary("Here you go: " + JSON.stringify(body)), expected);
});

test("parseSummary falls back to the raw text when it isn't the JSON shape, and rejects empty", () => {
  assert.deepEqual(plan.parseSummary("Just prose."), { raw: "Just prose." });
  assert.deepEqual(plan.parseSummary('{"unrelated":1}'), { raw: '{"unrelated":1}' });
  assert.throws(() => plan.parseSummary("  "), /empty/i);
});

test("summary cache round-trips, is keyed by issue, and rejects bad entries", () => {
  const os = require("node:os");
  const fs = require("node:fs");
  const path = require("node:path");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "sc-cache-"));
  try {
    assert.equal(plan.readSummaryCache("PROJ-1", root), null);
    const entry = { issueKey: "proj-1", commentCount: 4, omitted: 0, summary: { tldr: "x" }, completedAt: "2026-10-03T10:00:00.000Z" };
    plan.writeSummaryCache(entry, root);
    assert.deepEqual(plan.readSummaryCache("PROJ-1", root), { ...entry, issueKey: "PROJ-1" });
    assert.equal(plan.readSummaryCache("PROJ-2", root), null);
    assert.throws(() => plan.readSummaryCache("../etc", root), /issue key/i);
    assert.throws(() => plan.writeSummaryCache({ issueKey: "PROJ-1" }, root), /missing/i);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
