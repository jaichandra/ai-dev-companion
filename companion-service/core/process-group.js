// Whether a headless `claude` from an earlier run of this service is still
// alive. core/exec.ts starts claude detached, so it leads its own process
// group — which also means it outlives this process when that dies without
// a chance to clean up (a crash, `kill -9`). Both worktree features record
// that group id as `data.claudePid` while Claude runs, and after a restart
// Refresh diff and Approve use this to refuse, rather than stage, commit
// and push a worktree that orphaned Claude may still be writing.
//
// Only ever a probe (signal 0), never a kill: a pid read back from a job
// file after a restart may by now belong to an unrelated process, so the
// fail-closed answer is to refuse and let the user wait or Discard.
//
// Plain JS, not TypeScript, for the same reason as core/paths.js: no build
// step needed to unit test it, and the features' TypeScript requires it
// directly.

/**
 * True if process group `pgid` still has a live member. `kill(-pgid, 0)`
 * succeeding, or failing with EPERM (it exists but belongs to someone
 * else), means alive; ESRCH means no such group. A pgid that isn't a
 * positive integer is always false — 0 or a negative number would make
 * kill(2) address this process's own group or every process instead.
 */
function isProcessGroupAlive(pgid) {
  if (!Number.isInteger(pgid) || pgid <= 0) return false;
  try {
    process.kill(-pgid, 0);
    return true;
  } catch (err) {
    return err.code === "EPERM";
  }
}

module.exports = { isProcessGroupAlive };
