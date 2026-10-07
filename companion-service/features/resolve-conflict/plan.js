// Pure logic for resolve-conflict. Plain JS, not TypeScript — same reason
// as analyze-issue/plan.js: lets plan.test.js run this directly with zero
// build step; tsc still picks it up (allowJs) and copies it into dist/
// for index.ts's runtime use after a build.

/**
 * Splits a NUL-terminated list — the output of `git diff --name-only -z`
 * — into paths. NUL (not newline) matters here: git's two-column
 * `--porcelain` text format breaks on a rename ("R  old -> new" — the
 * arrow isn't a delimiter, it's literal text sharing the line with two
 * real paths) and C-quotes any path containing a space or a non-ASCII
 * byte, both confirmed by reproducing them — so nothing here parses that
 * format. `-z` output has none of that: paths are raw bytes,
 * NUL-terminated, no quoting.
 */
function splitNulSeparated(text) {
  return text.split("\0").filter(Boolean);
}

/**
 * The intersection of two NUL-terminated path lists, deduped, in the
 * order paths appear in `bZ`. Backs index.ts's reviewFiles(tree): a file
 * that differs from BOTH merge parents — `git diff --name-only -z
 * preMergeSha tree` intersected with `git diff --name-only -z destSha
 * tree` — actually combines content from both sides of the merge (a
 * resolved conflict, an auto-merged file, or a stray new file). A file
 * that differs from only ONE parent came through untouched from the
 * other side and was never really "merged" — nothing to review there.
 */
function intersectNulSeparated(aZ, bZ) {
  const a = new Set(splitNulSeparated(aZ));
  const seen = new Set();
  const result = [];
  for (const file of splitNulSeparated(bZ)) {
    if (a.has(file) && !seen.has(file)) {
      seen.add(file);
      result.push(file);
    }
  }
  return result;
}

/**
 * Whether approve() should skip `git commit` and push HEAD as-is. True
 * only when:
 *  - stagedClean: the index already matches HEAD exactly (`git diff
 *    --cached --quiet` exits 0) — nothing left to add to a commit;
 *  - aheadCount > 0: HEAD has moved past preMergeSha at all
 *    (`git rev-list --count preMergeSha..HEAD`);
 *  - isAncestor: and that move is a real ancestry relationship, not just
 *    "some commits differ" (`git merge-base --is-ancestor preMergeSha
 *    HEAD`).
 * All three together mean the merge (or the user's own commit, made in a
 * resumed terminal session) is already committed on top of preMergeSha
 * and there's nothing left to add — `git commit` here would just fail
 * with "nothing to commit".
 */
function shouldSkipCommit({ stagedClean, aheadCount, isAncestor }) {
  return stagedClean === true && aheadCount > 0 && isAncestor === true;
}

/**
 * Whether there's nothing at all to push: the index matches HEAD AND HEAD
 * never moved past preMergeSha — the merge itself was "Already up to
 * date" (or produced no real change) and nothing else changed either.
 * Checked before shouldSkipCommit — this is a different, more final
 * outcome (approve without ever running `git push`), not just "skip the
 * commit but still push".
 */
function isNothingToPush({ stagedClean, aheadCount }) {
  return stagedClean === true && aheadCount === 0;
}

/**
 * Decides which base to diff a CONFLICTED file against, and what to tell
 * the user, when it was resolved wholesale to one side rather than
 * actually combining both sides' changes:
 *  - isEmptyVsPreMerge (no difference from preMergeSha — resolved
 *    wholesale to the PR branch's own side): a diff against preMergeSha
 *    would be empty and would hide that the destination branch's changes
 *    were dropped for this file entirely — diff against destSha instead,
 *    with a note saying so.
 *  - equalsDest (no difference from destSha — resolved wholesale to the
 *    destination branch's side): the normal diff against preMergeSha
 *    already shows the change; just add a note saying so.
 *  - neither: both sides actually combined (or the file is genuinely
 *    unchanged, which can't reach this function as a "conflicted" file
 *    anyway) — diff against preMergeSha, no note.
 * isEmptyVsPreMerge is checked first, so the degenerate case where both
 * are true (preMergeSha, destSha and the resolved content all match) is
 * reported as "kept the PR branch's version" rather than "took the
 * destination's" — an arbitrary but deterministic tie-break.
 */
function classifyConflictedFile({ path, isEmptyVsPreMerge, equalsDest }) {
  if (isEmptyVsPreMerge) {
    return {
      path,
      diffAgainst: "dest",
      note: "Kept the PR branch's version — the destination branch's changes to this file were dropped.",
    };
  }
  if (equalsDest) {
    return {
      path,
      diffAgainst: "preMerge",
      note: "Took the destination branch's version — the PR branch's changes to this file were replaced.",
    };
  }
  return { path, diffAgainst: "preMerge", note: undefined };
}

// canReject lives in core/reviewed-push-policy.js (shared with
// address-review-comments); re-exported here so this feature's plan and
// its tests keep covering the one definition.
const { canReject } = require("../../core/reviewed-push-policy.js");

module.exports = {
  splitNulSeparated,
  intersectNulSeparated,
  shouldSkipCommit,
  isNothingToPush,
  classifyConflictedFile,
  canReject,
};
