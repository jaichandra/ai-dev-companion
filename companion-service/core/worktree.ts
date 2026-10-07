import * as fs from "fs";
import * as path from "path";
import { git, run } from "./exec";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("./paths.js") as { stateDir(): string };

export interface WorktreeHandle {
  /** Job/feature-scoped id, used only for the directory name. */
  id: string;
  /** Absolute path to the worktree's checkout. */
  dir: string;
  /** The local clone (main working tree) this worktree was branched from. */
  repoPath: string;
}

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
