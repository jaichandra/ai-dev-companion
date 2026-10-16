// Where should a feature work on a branch? Look for something that already
// exists before making anything: first a worktree (or the clone itself) that
// has the branch checked out, then a local or origin branch of that name.
// Nothing here creates a worktree unless createBranchWorktree is called, and
// the caller does that only after the person confirms. Plain JS so node:test
// runs it against real throwaway repos; every git call is an argv array.
const fs = require("fs");
const path = require("path");
const {
  defaultGit,
  parseWorktreePorcelain,
  parseForEachRef,
  FOR_EACH_REF_FORMAT,
  keyPattern,
  countChanged,
} = require("./ticket-git.js");
const { repoWorktreesRoot } = require("./paths.js");
const { linkNodeModules } = require("./review-worktree.js");
const { isSafeBranchName } = require("./bitbucket-endpoints.js");

/** Operations that leave a worktree half-way through something; git keeps a
 * marker for each under the worktree's own git dir. */
const IN_PROGRESS_MARKERS = [
  ["MERGE_HEAD", "merge"],
  ["rebase-merge", "rebase"],
  ["rebase-apply", "rebase"],
  ["CHERRY_PICK_HEAD", "cherry-pick"],
  ["REVERT_HEAD", "revert"],
];

function real(p) {
  try {
    return fs.realpathSync(p);
  } catch {
    return path.resolve(p);
  }
}

/** Uncommitted changes and any half-finished merge/rebase in `dir`. */
async function inspectWorktree(dir, { git = defaultGit } = {}) {
  const st = await git(["status", "--porcelain"], dir, { allowFailure: true });
  const changed = st.code === 0 ? countChanged(st.stdout) : 0;
  let inProgress = null;
  for (const [marker, name] of IN_PROGRESS_MARKERS) {
    const p = await git(["rev-parse", "--git-path", marker], dir, { allowFailure: true });
    if (p.code !== 0) continue;
    const file = path.resolve(dir, p.stdout.trim());
    if (fs.existsSync(file)) {
      inProgress = name;
      break;
    }
  }
  return { dirty: changed > 0, changed, inProgress };
}

async function refExists(repoPath, ref, git) {
  const r = await git(["show-ref", "--verify", "--quiet", ref], repoPath, { allowFailure: true });
  return r.code === 0;
}

/** The branch names to consider: the one asked for, else every branch whose
 * name carries the ticket key. */
async function candidateBranches(repoPath, { branch, issueKey }, git) {
  if (branch) {
    if (!isSafeBranchName(branch)) throw new Error(`Not a safe branch name: ${String(branch).slice(0, 80)}`);
    return [branch];
  }
  if (!issueKey) throw new Error("resolveWorkspace needs a branch or an issue key.");
  keyPattern(issueKey); // validates the key
  const refs = await git(["for-each-ref", `--format=${FOR_EACH_REF_FORMAT}`, "refs/heads", "refs/remotes/origin"], repoPath);
  return parseForEachRef(refs.stdout, issueKey).map((b) => b.name);
}

/**
 * Finds the place to work on a branch (the PR's source branch) or, with only
 * an issue key, on the ticket's branches. Returns one of:
 *   { kind: "worktree", dir, branch, isMainClone, dirty, changed, inProgress, usable, reason }
 *       an existing checkout of the branch (any path, the clone included);
 *       `usable` is false when it has uncommitted changes or a merge/rebase
 *       in progress, and `reason` says which.
 *   { kind: "branch", branch, local, remote }
 *       the branch exists but is checked out nowhere; making a worktree for
 *       it needs the person's confirmation.
 *   { kind: "none", branch }
 *       no such branch here (or, for a key, no branch at all).
 * `alternatives` lists the other candidate branches when a key matched several.
 * Never creates or changes anything.
 */
async function resolveWorkspace(repoPath, { branch = null, issueKey = null } = {}, { git = defaultGit } = {}) {
  const names = await candidateBranches(repoPath, { branch, issueKey }, git);
  const list = await git(["worktree", "list", "--porcelain"], repoPath);
  const main = real(repoPath);
  const worktrees = parseWorktreePorcelain(list.stdout)
    .filter((wt) => wt.branch && fs.existsSync(wt.dir))
    .map((wt) => ({ ...wt, isMainClone: real(wt.dir) === main }));

  for (const name of names) {
    const wt = worktrees.find((w) => w.branch === name);
    if (!wt) continue;
    const state = await inspectWorktree(wt.dir, { git });
    const reason = state.inProgress
      ? `a ${state.inProgress} is in progress in ${wt.dir}`
      : state.dirty
        ? `${wt.dir} has ${state.changed} uncommitted change${state.changed === 1 ? "" : "s"}`
        : null;
    return {
      kind: "worktree",
      dir: wt.dir,
      branch: name,
      isMainClone: wt.isMainClone,
      ...state,
      usable: !reason,
      reason,
      alternatives: names.filter((n) => n !== name),
    };
  }

  for (const name of names) {
    const local = await refExists(repoPath, `refs/heads/${name}`, git);
    const remote = await refExists(repoPath, `refs/remotes/origin/${name}`, git);
    if (local || remote) return { kind: "branch", branch: name, local, remote, alternatives: names.filter((n) => n !== name) };
  }
  return { kind: "none", branch: branch || null, alternatives: [] };
}

/** `<repo>.worktrees/<branch with / and other odd characters as ->`. */
function branchWorktreePath(repoPath, branch) {
  return path.join(repoWorktreesRoot(repoPath), branch.replace(/[^A-Za-z0-9._-]/g, "-"));
}

/**
 * Makes a worktree for an existing local or origin branch. Call it only after
 * the person confirmed (resolveWorkspace said kind "branch"). It never makes a
 * new branch: starting one for a ticket is ensureTicketWorktree's job. A
 * folder already at the path is never touched.
 */
async function createBranchWorktree(repoPath, branch, { git = defaultGit } = {}) {
  if (!isSafeBranchName(branch)) throw new Error(`Not a safe branch name: ${String(branch).slice(0, 80)}`);
  await git(["check-ref-format", "--branch", branch], repoPath);
  const dir = branchWorktreePath(repoPath, branch);
  if (fs.existsSync(dir)) throw new Error(`${dir} already exists. Move it aside and try again.`);
  const hadLocal = await refExists(repoPath, `refs/heads/${branch}`, git);
  await git(["fetch", "origin", `refs/heads/${branch}:refs/remotes/origin/${branch}`], repoPath, { allowFailure: hadLocal });
  const hasRemote = await refExists(repoPath, `refs/remotes/origin/${branch}`, git);
  if (!hadLocal && !hasRemote) throw new Error(`Branch ${branch} doesn't exist locally or on origin.`);
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  if (hadLocal) await git(["worktree", "add", dir, branch], repoPath);
  else await git(["worktree", "add", "--track", "-b", branch, dir, `origin/${branch}`], repoPath);
  linkNodeModules(repoPath, dir);
  return { dir, branch, created: true };
}

module.exports = { inspectWorktree, resolveWorkspace, branchWorktreePath, createBranchWorktree };
