// Provisions and locates each repo's dedicated PR-review worktree — a
// persistent fixture at ~/.ai-dev-companion/<repo>.worktrees/pr-review, not beside the repo's
// own clone (the same convention already used by hand for repos like
// sample-app: ~/gitviews/sample-app -> ~/.ai-dev-companion/sample-app.worktrees/pr-review).
//
// This is a *different* lifecycle from core/worktree.ts's addWorktree/
// removeWorktree: those are per-job, rooted under
// ~/.ai-dev-companion/worktrees, detached by design, and removed on
// approve/reject. This worktree is provisioned once per repo and never
// removed — review-in-editor re-checks it out to a different branch on
// every click instead of creating a new one each time.
//
// Plain JS, not TypeScript — same reason as core/prereqs.js: setup.js
// requires this directly with zero build step (to provision worktrees
// right after writing config.json, before `npm run build` has ever run).
// Its own local exec() below exists instead of importing core/exec.ts for
// the same reason — that file is TypeScript-only and doesn't exist as
// plain JS until after a build.
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");
const { repoWorktreesRoot } = require("./paths.js");

function exec(command, args, opts = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd: opts.cwd, shell: false });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0 && !opts.allowFailure) {
        reject(new Error(`${command} ${args.join(" ")} exited with ${code}: ${stderr.trim() || stdout.trim()}`));
        return;
      }
      resolve({ stdout, stderr, code });
    });
  });
}

function git(args, cwd, opts = {}) {
  return exec("git", args, Object.assign({ cwd }, opts));
}

/** `~/.ai-dev-companion/<basename>.worktrees/pr-review`. */
function reviewWorktreePath(repoPath) {
  return path.join(repoWorktreesRoot(repoPath), "pr-review");
}

/**
 * True if `dir` is already registered as one of repoPath's own worktrees —
 * regardless of what branch it's currently on, or whether it's dirty. The
 * only thing that matters for idempotency: a worktree the user already
 * checked out by hand (e.g. mid-review) must never be recreated out from
 * under them.
 */
async function isRegisteredWorktree(repoPath, dir) {
  const { stdout } = await git(["worktree", "list", "--porcelain"], repoPath, { allowFailure: true });
  const target = fs.existsSync(dir) ? fs.realpathSync(dir) : path.resolve(dir);
  for (const line of stdout.split("\n")) {
    if (!line.startsWith("worktree ")) continue;
    const candidate = line.slice("worktree ".length).trim();
    if (fs.existsSync(candidate) && fs.realpathSync(candidate) === target) {
      return true;
    }
  }
  return false;
}

/**
 * `git worktree add` never gives you `node_modules` — it's gitignored, and
 * each worktree is a separate checkout. Symlinked (not copied — a real
 * clone's node_modules can be well over 1GB) so editor tooling (a TS
 * language server, lint, etc.) actually works in the review worktree.
 * Mirrors core/worktree.ts's own linkNodeModules; duplicated rather than
 * shared for the same "plain JS, no build step" reason as exec() above.
 */
function linkNodeModules(repoPath, worktreeDir) {
  const source = path.join(repoPath, "node_modules");
  const dest = path.join(worktreeDir, "node_modules");
  if (fs.existsSync(source) && !fs.existsSync(dest)) {
    fs.symlinkSync(source, dest, "dir");
  }
}

/** The ref to land a fresh review worktree on before any real PR branch is
 * ever checked out into it — origin/HEAD (the repo's actual default
 * branch) if the clone knows it, else the two conventional fallbacks. */
async function defaultRemoteRef(repoPath) {
  const symbolic = await git(["symbolic-ref", "refs/remotes/origin/HEAD"], repoPath, { allowFailure: true });
  if (symbolic.code === 0 && symbolic.stdout.trim()) {
    return symbolic.stdout.trim().replace(/^refs\/remotes\//, "");
  }
  for (const candidate of ["origin/master", "origin/main"]) {
    const check = await git(["rev-parse", "--verify", candidate], repoPath, { allowFailure: true });
    if (check.code === 0) return candidate;
  }
  throw new Error(
    `Could not determine a default branch for ${repoPath} (no origin/HEAD, origin/master, or origin/main).`,
  );
}

/**
 * Idempotent: a no-op if `reviewWorktreePath(repoPath)` is already a
 * registered worktree of this repo, on whatever branch it happens to be on
 * right now — review-in-editor's own start() re-checks it out to the PR's
 * actual branch on every click, so provisioning only ever needs to get a
 * *valid* worktree into existence once.
 */
async function provisionReviewWorktree(repoPath) {
  const dir = reviewWorktreePath(repoPath);
  if (await isRegisteredWorktree(repoPath, dir)) {
    return { dir, created: false };
  }

  await git(["fetch", "origin", "--prune"], repoPath);
  const ref = await defaultRemoteRef(repoPath);
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  // A stale, non-worktree leftover at this exact path (e.g. a directory
  // from a previous failed run) would make `git worktree add` fail with a
  // confusing "already exists" error — clear it first.
  if (fs.existsSync(dir)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
  // The directory may have been deleted out from under git (cleanup, purge
  // of the data folder) while `.git/worktrees/<id>` still registers it —
  // `worktree add` then refuses with "missing but already registered".
  await git(["worktree", "prune"], repoPath);
  await git(["worktree", "add", "--detach", dir, ref], repoPath);
  linkNodeModules(repoPath, dir);
  return { dir, created: true };
}

module.exports = {
  reviewWorktreePath,
  isRegisteredWorktree,
  provisionReviewWorktree,
  linkNodeModules,
};
