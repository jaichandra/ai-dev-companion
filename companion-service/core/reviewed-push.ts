// The "Claude edited a disposable worktree, a human reviewed the diff, now
// push exactly that" plumbing shared by every feature that pushes a
// reviewed worktree: resolve-conflict (where it was born — moved here
// verbatim) and address-review-comments. Kept in one place so the two
// can't drift on the parts that actually protect the user: the per-job
// lock, what "stage everything" means, the tree fingerprint, and when a
// job may still be discarded.
import * as fs from "fs";
import * as path from "path";
import { git } from "./exec";

// canReject is pure logic, kept in plain JS (core/reviewed-push-policy.js)
// so features/*/plan.js can re-export it and its tests keep covering it.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const policy = require("./reviewed-push-policy.js") as { canReject(status: string): boolean };

/** Whether a job in `status` may be discarded (reject/cancel-after-finish):
 * only once it has settled into something reviewable or failed — never
 * mid-approve/mid-reject. */
export function canReject(status: string): boolean {
  return policy.canReject(status);
}

/** Serializes approve() and refreshDiff() for the same job id: both stage
 * the whole worktree, fingerprint it, and mutate the job's status/data,
 * so two concurrent calls for the same job (e.g. a Refresh diff click
 * racing an Approve click) must never interleave their git operations.
 * Each call chains onto whatever's already queued for this job id, so
 * calls for the SAME job run strictly one at a time, in order; calls for
 * different job ids never wait on each other. */
const jobLocks = new Map<string, Promise<void>>();

export function withJobLock<T>(jobId: string, fn: () => Promise<T>): Promise<T> {
  const prior = jobLocks.get(jobId) ?? Promise.resolve();
  const result = prior.then(fn, fn);
  jobLocks.set(
    jobId,
    result.then(
      () => undefined,
      () => undefined,
    ),
  );
  return result;
}

/** Stages the ENTIRE worktree — no pathspec list, so no glob/pathspec
 * edge case (a rename, a space, a quoted path) can be gotten wrong — then
 * unstages the node_modules symlink core/worktree.ts's linkNodeModules
 * creates: confirmed empirically that a gitignore directory pattern like
 * "/node_modules/" does NOT match a symlink (only a real directory is),
 * so `-A` always picks it up as untracked. Checking existence first (and
 * skipping the reset entirely when there's none) means a REAL reset
 * failure — anything other than "there was nothing to unstage" — is never
 * swallowed; it throws git's own actionable stderr instead. */
export async function stageAll(worktreeDir: string): Promise<void> {
  await git(["add", "-A"], worktreeDir);
  if (fs.existsSync(path.join(worktreeDir, "node_modules"))) {
    await git(["reset", "-q", "--", "node_modules"], worktreeDir);
  }
}

/** The worktree's current tree hash — a fingerprint of the ENTIRE tree
 * (not just whichever files were last staged), taken right after
 * stageAll so it reflects everything in the worktree. */
export async function writeTree(worktreeDir: string): Promise<string> {
  return (await git(["write-tree"], worktreeDir)).stdout.trim();
}

/** Approve must push exactly what the panel last showed. Comparing the
 * WHOLE tree's hash (not just "did staging find anything") catches any
 * change since the last render, anywhere in the worktree — not only
 * within whatever was last staged. */
export function assertTreeMatchesReviewed(tree: string, reviewedTree: string | undefined): void {
  if (tree !== reviewedTree) {
    throw new Error(
      "The worktree changed since you reviewed it — click Refresh diff, check the changes, then approve.",
    );
  }
}
