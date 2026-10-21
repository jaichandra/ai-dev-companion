const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { readReview, writeReview } = require("./review-memory.js");

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "review-memory-"));

test("a written review reads back, and an unknown key or missing file is null", () => {
  const dir = tmp();
  assert.equal(readReview("P/r#1", dir), null);
  const rec = { sessionId: "s", cwd: "/w", reviewedSha: "abc1234", reviewedAt: 100 };
  writeReview("P/r#1", rec, dir, 100);
  assert.deepEqual(readReview("P/r#1", dir), rec);
  assert.equal(readReview("P/r#2", dir), null);
});

test("writing replaces that PR's entry, keeps others and drops stale ones", () => {
  const dir = tmp();
  const day = 24 * 60 * 60 * 1000;
  writeReview("old", { sessionId: "s", cwd: "/w", reviewedSha: "a", reviewedAt: 0 }, dir, 0);
  writeReview("keep", { sessionId: "s", cwd: "/w", reviewedSha: "b", reviewedAt: 10 * day }, dir, 10 * day);
  writeReview("keep", { sessionId: "t", cwd: "/w", reviewedSha: "c", reviewedAt: 40 * day }, dir, 40 * day);
  assert.equal(readReview("old", dir), null);
  assert.equal(readReview("keep", dir).sessionId, "t");
});

test("a corrupt file reads as empty", () => {
  const dir = tmp();
  fs.writeFileSync(path.join(dir, "review-memory.json"), "{nope");
  assert.equal(readReview("x", dir), null);
});
