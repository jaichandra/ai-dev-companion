// Whether a persisted job's recorded Claude-session directory is somewhere
// this service itself would have run Claude for a job with no worktree: a
// mapped repo clone, one of analyze-issue's read-only checkouts
// (stateDir/repos/<PROJECT>/<repo>), a folder inside a
// mapped repo's `<stateDir>/<repo>.worktrees/` (Ticket to PR's
// `<stateDir>/<repo>.worktrees/<KEY>`), or a per-ticket directory under
// stateDir/sessions. A job file is untrusted input, and "Continue in Claude
// Code" opens a terminal in this directory — a hand-edited file must not be
// able to point it at an attacker's folder (whose .claude settings would
// then load).
const path = require("path");
const { repoWorktreesRoot } = require("./paths.js");

function strictlyInside(root, target) {
  if (typeof root !== "string" || typeof target !== "string") return false;
  const rel = path.relative(path.resolve(root), path.resolve(target));
  return rel !== "" && !rel.startsWith("..") && !path.isAbsolute(rel);
}

/** `<repo>.worktrees/<one folder>` — exactly one level down, so the
 * `.worktrees` folder itself (or anything deeper) never counts. */
function isRepoWorktreeDir(repoPath, resolved) {
  const root = repoWorktreesRoot(repoPath);
  return strictlyInside(root, resolved) && path.dirname(resolved) === root;
}

function sessionCwdAllowed(cwd, { repoPaths, sessionsRoot, checkoutsRoot }) {
  if (typeof cwd !== "string" || !path.isAbsolute(cwd)) return false;
  const resolved = path.resolve(cwd);
  const repos = (repoPaths || []).filter((p) => typeof p === "string");
  if (repos.some((p) => path.resolve(p) === resolved)) return true;
  if (repos.some((p) => isRepoWorktreeDir(p, resolved))) return true;
  // core/analysis-checkout.js's read-only checkouts: <checkoutsRoot>/<PROJECT>/<repo>.
  if (strictlyInside(checkoutsRoot, resolved)) {
    const rel = path.relative(path.resolve(checkoutsRoot), resolved).split(path.sep);
    if (rel.length === 2) return true;
  }
  return strictlyInside(sessionsRoot, resolved);
}

module.exports = { sessionCwdAllowed, strictlyInside };
