// What `companion worktrees` shows and removes: every worktree of the configured repos, whether this tool
// made it, and what is safe to clear. Plain JS with git and the filesystem passed in, so node:test runs it
// against real throwaway repos. A worktree is the tool's only when it sits under `<repo>.worktrees/` or the
// job-worktree root; your own (and the clone itself) are listed and never removed.
const fs = require("fs");
const path = require("path");
const { parseWorktreePorcelain } = require("./ticket-git.js");
const { repoWorktreesRoot } = require("./paths.js");
const { strictlyInside } = require("./session-cwd.js");
const { inspectWorktree } = require("./workspace-resolver.js");

/** How many worktrees the tool may have in one repo before Start fix says to clear some first. */
const WORKTREE_SOFT_LIMIT = 5;

function real(p) {
  try {
    return fs.realpathSync(p);
  } catch {
    return path.resolve(p);
  }
}

function ownedBy(repoPath, dir, jobWorktreesRoot) {
  return strictlyInside(repoWorktreesRoot(repoPath), dir) || (!!jobWorktreesRoot && strictlyInside(jobWorktreesRoot, dir));
}

/** origin's default branch ref (`origin/master`), or null. */
async function defaultRef(repoPath, git) {
  const head = await git(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"], repoPath, { allowFailure: true });
  if (head.code === 0 && head.stdout.trim()) return head.stdout.trim().replace(/^refs\/remotes\//, "");
  for (const name of ["master", "main"]) {
    const r = await git(["show-ref", "--verify", "--quiet", `refs/remotes/origin/${name}`], repoPath, { allowFailure: true });
    if (r.code === 0) return `origin/${name}`;
  }
  return null;
}

/**
 * The worktrees of `repos` ({ key: path }), each with its state:
 * `{ repoKey, repoPath, dir, branch, isMainClone, owned, exists, dirty, changed, inProgress, unpushed, merged,
 *    upstreamGone, issueKey, prKey, sessions, lastUsed, prunable, why }`. `units` are the work-unit records
 * (core/work-units.js) that supply the ticket, PR, sessions and last use.
 */
async function listWorktrees({ repos, units = [], git, jobWorktreesRoot = null }) {
  const rows = [];
  for (const [repoKey, repoPath] of Object.entries(repos || {})) {
    if (!fs.existsSync(path.join(repoPath, ".git"))) continue;
    const list = await git(["worktree", "list", "--porcelain"], repoPath, { allowFailure: true });
    if (list.code !== 0) continue;
    const base = await defaultRef(repoPath, git);
    for (const wt of parseWorktreePorcelain(list.stdout)) {
      const isMainClone = real(wt.dir) === real(repoPath);
      const exists = fs.existsSync(wt.dir);
      const owned = !isMainClone && ownedBy(repoPath, wt.dir, jobWorktreesRoot);
      const unit = units.find((u) => u.dir && real(u.dir) === real(wt.dir)) || null;
      const row = {
        repoKey,
        repoPath,
        dir: wt.dir,
        branch: wt.branch,
        isMainClone,
        owned,
        exists,
        dirty: false,
        changed: 0,
        inProgress: null,
        unpushed: 0,
        merged: false,
        upstreamGone: false,
        issueKey: unit ? unit.issueKey : null,
        prKey: unit ? unit.prKey : null,
        sessions: unit ? unit.sessions.length : 0,
        lastUsed: unit ? unit.updatedAt : exists ? fs.statSync(wt.dir).mtimeMs : null,
        prunable: false,
        why: null,
      };
      if (exists) {
        Object.assign(row, await inspectWorktree(wt.dir, { git }));
        const local = await git(["rev-list", "--count", "HEAD", "--not", "--remotes"], wt.dir, { allowFailure: true });
        row.unpushed = local.code === 0 ? Number(local.stdout.trim()) || 0 : 0;
        if (base) {
          const anc = await git(["merge-base", "--is-ancestor", "HEAD", base], wt.dir, { allowFailure: true });
          row.merged = anc.code === 0;
        }
        if (wt.branch) {
          const track = await git(["for-each-ref", "--format=%(upstream:track)", `refs/heads/${wt.branch}`], repoPath, { allowFailure: true });
          row.upstreamGone = track.stdout.includes("gone");
        }
      }
      row.why = pruneVerdict(row);
      row.prunable = row.why === null && owned;
      rows.push(row);
    }
  }
  return rows;
}

/** null when the worktree is safe to remove, otherwise the reason it isn't. */
function pruneVerdict(row) {
  if (row.isMainClone) return "your clone";
  if (!row.owned) return "yours, not made by the tool";
  if (!row.exists) return "folder is gone";
  if (row.inProgress) return `a ${row.inProgress} is in progress`;
  if (row.dirty) return `${row.changed} uncommitted change${row.changed === 1 ? "" : "s"}`;
  // Removing a worktree never deletes its branch, so commits are not lost either way. A branch whose
  // upstream is gone was pushed and then deleted (the usual end of a merged PR, squash included), and one
  // that is an ancestor of the default branch is merged; anything else is still work in progress.
  if (!row.merged && !row.upstreamGone) {
    return row.unpushed > 0 ? `${row.unpushed} commit${row.unpushed === 1 ? "" : "s"} not pushed` : "branch not merged";
  }
  return null;
}

/** Removes the prunable worktrees (git refuses a dirty one itself, so there is no --force) and drops stale
 * registrations. Returns `{ removed, failed }`. */
async function pruneWorktrees(rows, { git, workUnits = null }) {
  const removed = [];
  const failed = [];
  for (const row of rows.filter((r) => r.prunable)) {
    const r = await git(["worktree", "remove", row.dir], row.repoPath, { allowFailure: true });
    if (r.code === 0) removed.push(row);
    else failed.push({ row, error: (r.stderr || r.stdout).trim().split("\n")[0] });
  }
  for (const repoPath of new Set(rows.map((r) => r.repoPath))) await git(["worktree", "prune"], repoPath, { allowFailure: true });
  if (workUnits) workUnits.prune((u) => !u.dir || fs.existsSync(u.dir) || u.sessions.length > 0);
  return { removed, failed };
}

/** How many worktrees of `repoPath` the tool owns now. */
async function countOwned({ repoPath, git, jobWorktreesRoot = null }) {
  const list = await git(["worktree", "list", "--porcelain"], repoPath, { allowFailure: true });
  if (list.code !== 0) return 0;
  return parseWorktreePorcelain(list.stdout).filter((wt) => real(wt.dir) !== real(repoPath) && ownedBy(repoPath, wt.dir, jobWorktreesRoot)).length;
}

module.exports = { WORKTREE_SOFT_LIMIT, listWorktrees, pruneVerdict, pruneWorktrees, countOwned };
