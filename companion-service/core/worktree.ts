import * as fs from "fs";
import * as path from "path";
import { git, run } from "./exec";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("./paths.js") as { stateDir(): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const workspaceResolver = require("./workspace-resolver.js") as {
  resolveWorkspace(
    repoPath: string,
    target: { branch: string },
  ): Promise<
    | { kind: "worktree"; dir: string; isMainClone: boolean; usable: boolean; reason: string | null }
    | { kind: "branch" | "none" }
  >;
};

export interface WorktreeHandle {
  /** Job/feature-scoped id, used only for the directory name. */
  id: string;
  /** Absolute path to the worktree's checkout. */
  dir: string;
  /** The local clone (main working tree) this worktree was branched from. */
  repoPath: string;
  /** True for a worktree that already existed (acquireWorktree reused it):
   * it is not this job's to delete, and rejecting the job restores it
   * instead. Absent for the job's own disposable worktree. */
  adopted?: boolean;
  /** The branch checked out there, for an adopted worktree. */
  branch?: string;
}

/** An existing checkout of the branch was found but can't be used safely. */
export class WorkspaceUnusableError extends Error {}

// Worktrees live outside any git-tracked directory (under the state dir,
// not inside `repoPath`) so they never show up as untracked clutter in
// `git status` on the user's real checkout. Exported so server.ts's job
// persistence (core/job-files.js's loadJobs) validates a loaded job's
// `data.worktree.dir` against this exact same root, rather than
// rebuilding the path and risking it drifting out of sync.
export const WORKTREE_ROOT = path.join(paths.stateDir(), "worktrees");

function slugify(input: string): string {
  return input.replace(/[^a-zA-Z0-9_-]/g, "-");
}

/**
 * Fetch `origin`, then create a detached worktree checked out to
 * `origin/<branchRef>`. Detached (rather than a local branch) so approving
 * later is a plain `git push origin HEAD:<branchRef>` with no branch
 * bookkeeping to reconcile.
 */
export async function addWorktree(
  repoPath: string,
  branchRef: string,
  id: string,
): Promise<WorktreeHandle> {
  await git(["fetch", "origin", "--prune"], repoPath);

  const dir = path.join(WORKTREE_ROOT, slugify(path.basename(repoPath)), slugify(id));
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  // Guard against a stale leftover from a previous run under the same id.
  if (fs.existsSync(dir)) {
    await removeWorktreeAt(repoPath, dir);
  }

  await git(["worktree", "add", "--detach", dir, `origin/${branchRef}`], repoPath);
  linkNodeModules(repoPath, dir);
  return { id, dir, repoPath };
}

/**
 * `git worktree add` never gives you `node_modules` — it's gitignored, and
 * each worktree is a separate checkout. Without it, the main clone's husky
 * pre-commit hook (which shells out to `lint-staged`) fails with
 * "command not found" and blocks the merge commit on Approve — confirmed
 * by reproducing the exact failure. Symlinking (not copying — this clone's
 * node_modules is > 1GB) makes the worktree's tooling actually work; it's
 * safe because lint-staged/husky only read node_modules, never write to it.
 */
function linkNodeModules(repoPath: string, worktreeDir: string): void {
  const source = path.join(repoPath, "node_modules");
  const dest = path.join(worktreeDir, "node_modules");
  if (fs.existsSync(source) && !fs.existsSync(dest)) {
    fs.symlinkSync(source, dest, "dir");
  }
}

export async function removeWorktree(handle: WorktreeHandle): Promise<void> {
  // An adopted worktree was somebody's before this job and stays theirs.
  if (handle.adopted) return;
  await removeWorktreeAt(handle.repoPath, handle.dir);
}

async function removeWorktreeAt(repoPath: string, dir: string): Promise<void> {
  // --force: the worktree may still have an in-progress (aborted or not)
  // merge; we always want the directory gone once a job is done with it.
  await run("git", ["-C", repoPath, "worktree", "remove", "--force", dir], {
    allowFailure: true,
  });
  if (fs.existsSync(dir)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
  await git(["worktree", "prune"], repoPath);
}

/**
 * Where a job that edits `branchRef` should work, preferring what exists:
 * a linked worktree that already has the branch checked out is reused (after
 * a fetch, fast-forwarded to origin) as long as it is clean. Only when no
 * checkout of the branch exists is a disposable job worktree made, as
 * addWorktree does. Refuses, rather than falling back, when a checkout
 * exists but has uncommitted work, an operation in progress, commits that
 * aren't on origin, or is the main clone: Claude's sandbox is allowed to
 * write the worktree's own git dir, and for the main clone that is the whole
 * `.git` (hooks and config included).
 */
export async function acquireWorktree(
  repoPath: string,
  branchRef: string,
  id: string,
): Promise<{ handle: WorktreeHandle; reused: boolean }> {
  await git(["fetch", "origin", "--prune"], repoPath);
  const found = await workspaceResolver.resolveWorkspace(repoPath, { branch: branchRef });
  if (found.kind !== "worktree") return { handle: await addWorktree(repoPath, branchRef, id), reused: false };
  if (found.isMainClone) {
    throw new WorkspaceUnusableError(
      `The branch ${branchRef} is checked out in your main clone (${found.dir}). A job that lets Claude edit can't ` +
        "run there. Check out another branch in the clone, or make a worktree for this branch, and try again.",
    );
  }
  if (!found.usable) {
    throw new WorkspaceUnusableError(`Can't reuse the checkout of ${branchRef}: ${found.reason}. Commit or stash there, then try again.`);
  }
  const counts = (
    await git(["rev-list", "--left-right", "--count", `HEAD...origin/${branchRef}`], found.dir, { allowFailure: true })
  ).stdout
    .trim()
    .split(/\s+/)
    .map(Number);
  const [ahead, behind] = counts;
  if (ahead > 0) {
    throw new WorkspaceUnusableError(
      `Can't reuse the checkout of ${branchRef} at ${found.dir}: it has ${ahead} commit${ahead === 1 ? "" : "s"} that ` +
        "aren't on origin. Push or set them aside, then try again.",
    );
  }
  if (behind > 0) await git(["merge", "--ff-only", `origin/${branchRef}`], found.dir);
  return { handle: { id, dir: found.dir, repoPath, adopted: true, branch: branchRef }, reused: true };
}

/** Puts an adopted worktree back exactly as the job found it: aborts any merge,
 * resets to the commit it started at and removes files the job added. Only
 * called when the job is rejected; after an approved push the worktree is left
 * as it is. */
export async function restoreAdoptedWorktree(handle: WorktreeHandle, startSha: string): Promise<void> {
  if (!handle.adopted) return;
  await git(["merge", "--abort"], handle.dir, { allowFailure: true });
  await git(["reset", "--hard", startSha], handle.dir);
  await git(["clean", "-fd"], handle.dir);
}
