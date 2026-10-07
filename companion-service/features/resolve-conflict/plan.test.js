const test = require("node:test");
const assert = require("node:assert/strict");
const {
  splitNulSeparated,
  intersectNulSeparated,
  shouldSkipCommit,
  isNothingToPush,
  classifyConflictedFile,
  canReject,
} = require("./plan.js");

// ---- splitNulSeparated ----

test("splitNulSeparated splits on NUL and drops the trailing empty entry", () => {
  assert.deepEqual(splitNulSeparated("a.txt\0b.txt\0"), ["a.txt", "b.txt"]);
});

test("splitNulSeparated returns an empty array for empty input", () => {
  assert.deepEqual(splitNulSeparated(""), []);
});

// ---- intersectNulSeparated ----

test("intersectNulSeparated keeps only paths present in both lists", () => {
  const a = "shared.ts\0only-a.ts\0";
  const b = "shared.ts\0only-b.ts\0";
  assert.deepEqual(intersectNulSeparated(a, b), ["shared.ts"]);
});

test("intersectNulSeparated excludes a path that appears in only one list", () => {
  assert.deepEqual(intersectNulSeparated("only-a.ts\0", "only-b.ts\0"), []);
});

test("intersectNulSeparated dedupes a path repeated in the second list", () => {
  const a = "shared.ts\0";
  const b = "shared.ts\0shared.ts\0";
  assert.deepEqual(intersectNulSeparated(a, b), ["shared.ts"]);
});

test("intersectNulSeparated keeps a path with a space and a non-ASCII path", () => {
  const a = "docs/release notes.md\0src/café.ts\0";
  const b = "docs/release notes.md\0src/café.ts\0";
  assert.deepEqual(intersectNulSeparated(a, b), ["docs/release notes.md", "src/café.ts"]);
});

test("intersectNulSeparated returns an empty array when either input is empty", () => {
  assert.deepEqual(intersectNulSeparated("", "a.ts\0"), []);
  assert.deepEqual(intersectNulSeparated("a.ts\0", ""), []);
  assert.deepEqual(intersectNulSeparated("", ""), []);
});

// ---- shouldSkipCommit ----

test("shouldSkipCommit is true when staged is clean, HEAD is ahead, and preMergeSha is a real ancestor", () => {
  assert.equal(shouldSkipCommit({ stagedClean: true, aheadCount: 1, isAncestor: true }), true);
});

test("shouldSkipCommit is false when something is staged, even if HEAD is ahead and an ancestor", () => {
  assert.equal(shouldSkipCommit({ stagedClean: false, aheadCount: 1, isAncestor: true }), false);
});

test("shouldSkipCommit is false when HEAD hasn't moved past preMergeSha", () => {
  assert.equal(shouldSkipCommit({ stagedClean: true, aheadCount: 0, isAncestor: true }), false);
});

test("shouldSkipCommit is false when preMergeSha isn't an ancestor of HEAD", () => {
  assert.equal(shouldSkipCommit({ stagedClean: true, aheadCount: 1, isAncestor: false }), false);
});

// ---- isNothingToPush ----

test("isNothingToPush is true when staged is clean and HEAD never moved past preMergeSha", () => {
  assert.equal(isNothingToPush({ stagedClean: true, aheadCount: 0 }), true);
});

test("isNothingToPush is false when HEAD moved, even if staged is clean", () => {
  assert.equal(isNothingToPush({ stagedClean: true, aheadCount: 1 }), false);
});

test("isNothingToPush is false when something is staged, even if HEAD never moved", () => {
  assert.equal(isNothingToPush({ stagedClean: false, aheadCount: 0 }), false);
});

// ---- classifyConflictedFile ----

test("classifyConflictedFile: resolved wholesale to the PR branch's side renders against destSha, with a note", () => {
  const result = classifyConflictedFile({ path: "a.ts", isEmptyVsPreMerge: true, equalsDest: false });
  assert.equal(result.diffAgainst, "dest");
  assert.equal(
    result.note,
    "Kept the PR branch's version — the destination branch's changes to this file were dropped.",
  );
});

test("classifyConflictedFile: resolved wholesale to the destination branch's side renders against preMergeSha, with a note", () => {
  const result = classifyConflictedFile({ path: "a.ts", isEmptyVsPreMerge: false, equalsDest: true });
  assert.equal(result.diffAgainst, "preMerge");
  assert.equal(
    result.note,
    "Took the destination branch's version — the PR branch's changes to this file were replaced.",
  );
});

test("classifyConflictedFile: an actually-combined file renders against preMergeSha, no note", () => {
  const result = classifyConflictedFile({ path: "a.ts", isEmptyVsPreMerge: false, equalsDest: false });
  assert.equal(result.diffAgainst, "preMerge");
  assert.equal(result.note, undefined);
});

test("classifyConflictedFile: both empty (degenerate) prefers the PR-branch-side classification", () => {
  const result = classifyConflictedFile({ path: "a.ts", isEmptyVsPreMerge: true, equalsDest: true });
  assert.equal(result.diffAgainst, "dest");
});

test("classifyConflictedFile passes the path straight through", () => {
  const result = classifyConflictedFile({ path: "src/a.ts", isEmptyVsPreMerge: false, equalsDest: false });
  assert.equal(result.path, "src/a.ts");
});

// ---- canReject ----

test("canReject allows discarding a job awaiting approval", () => {
  assert.equal(canReject("awaiting-approval"), true);
});

test("canReject allows discarding a job that failed", () => {
  assert.equal(canReject("failed"), true);
});

test("canReject denies a job mid-approve", () => {
  assert.equal(canReject("approving"), false);
});

test("canReject denies a job already mid-reject", () => {
  assert.equal(canReject("rejecting"), false);
});

test("canReject denies a job that already approved (pushed)", () => {
  assert.equal(canReject("approved"), false);
});

test("canReject denies a job already rejected", () => {
  assert.equal(canReject("rejected"), false);
});

test("canReject denies a job still running", () => {
  assert.equal(canReject("running"), false);
});
