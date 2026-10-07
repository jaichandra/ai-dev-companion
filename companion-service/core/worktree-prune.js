// Removes the worktree of a job that aged out. The caller supplies the real
// removers (core/worktree.ts's removeWorktree and fs.rmSync) so this stays
// testable. A job flagged gitMetadataCompromised (resolve-conflict found the
// worktree's .git redirected) must never be handed to git again — it is
// deleted as a plain directory instead, exactly as that feature's own
// discard does.
const { strictlyInside } = require("./session-cwd.js");

async function removePrunedWorktree(job, { removeWorktree, rmSync, worktreeRoot }) {
  const handle = job && job.data ? job.data.worktree : undefined;
  if (!handle || typeof handle !== "object") return "none";
  // Defence in depth: loadJobs already checked this, but never delete outside the root.
  if (!strictlyInside(worktreeRoot, handle.dir)) return "skipped";
  if (job.data.gitMetadataCompromised) {
    rmSync(handle.dir, { recursive: true, force: true });
    return "removed-plain";
  }
  await removeWorktree(handle);
  return "removed";
}

module.exports = { removePrunedWorktree };
