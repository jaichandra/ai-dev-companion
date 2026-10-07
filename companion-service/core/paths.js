// The single source of truth for where this service's own state lives on
// disk, and the one-time move off the old (bitbucket-ai-companion-branded)
// directory name onto this one. Every other core/*.js and *.ts module gets
// the state dir from here — see core/updater.js's stateDir() and
// core/worktree.ts's WORKTREE_ROOT — rather than building `~/.something`
// paths by hand, so there is exactly one place that knows both names.
//
// Plain JS, not TypeScript, for the same reason as core/prereqs.js:
// setup.js and tests require this directly with no build step.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const { APP_SLUG } = require("./app-slug.js");

const STATE_DIR_NAME = `.${APP_SLUG}`;
const LEGACY_STATE_DIR_NAME = ".bitbucket-ai-companion";

function stateDir(home = os.homedir()) {
  return path.join(home, STATE_DIR_NAME);
}

/** `<stateDir>/<repo>.worktrees` — where every feature keeps a repo's
 * persistent worktrees (pr-review, <KEY>), rather than beside the clone. */
function repoWorktreesRoot(repoPath, home = os.homedir()) {
  return path.join(stateDir(home), `${path.basename(repoPath)}.worktrees`);
}

function legacyStateDir(home = os.homedir()) {
  return path.join(home, LEGACY_STATE_DIR_NAME);
}

/**
 * Runs `git` for migrateStateDir's worktree repairs. Synchronous — and kept
 * that way, rather than going through core/exec.ts's (async) git() — so
 * migrateStateDir itself can stay synchronous: a one-time startup step has
 * no need for async, and server.ts calling it synchronously (before
 * FEATURES is built) is far simpler than threading a Promise through an
 * otherwise entirely synchronous bootstrap.
 */
function defaultGit(args, cwd) {
  execFileSync("git", args, { cwd, stdio: "pipe", encoding: "utf8" });
}

/** `fs.renameSync`, falling back to a recursive copy-then-delete when the
 * two paths are on different filesystems (EXDEV) and a plain rename can't
 * cross them. */
function renameOrCopy(src, dest) {
  try {
    fs.renameSync(src, dest);
  } catch (err) {
    if (err.code !== "EXDEV") throw err;
    fs.cpSync(src, dest, { recursive: true });
    fs.rmSync(src, { recursive: true, force: true });
  }
}

/** Whether `p` names a live, followable directory — false, and never
 * throwing, for anything else, including a symlink whose target no
 * longer exists. A plain `fs.statSync` follows symlinks and throws ENOENT
 * on a dangling one; checking `lstatSync` first means a broken entry left
 * under `worktrees/` (by some external cleanup, a half-finished job, etc.)
 * is just skipped rather than aborting the whole scan. */
function isLiveDirectory(p) {
  let entry;
  try {
    entry = fs.lstatSync(p);
  } catch {
    return false;
  }
  if (!entry.isSymbolicLink()) return entry.isDirectory();
  try {
    return fs.statSync(p).isDirectory();
  } catch {
    return false;
  }
}

/** Every git worktree checkout under `<dir>/worktrees/<repo>/<id>/` (the
 * layout core/worktree.ts's addWorktree creates) — identified by a `.git`
 * *file* there (a real clone has a `.git` directory; a worktree's is a
 * one-line file pointing back at the main repo). Never throws on a broken
 * entry (see isLiveDirectory) — it's skipped, not fatal to the rest of the
 * scan. */
function findWorktreeDirs(dir) {
  const found = [];
  const worktreesRoot = path.join(dir, "worktrees");
  if (!fs.existsSync(worktreesRoot)) return found;
  for (const repo of fs.readdirSync(worktreesRoot)) {
    const repoDir = path.join(worktreesRoot, repo);
    if (!isLiveDirectory(repoDir)) continue;
    for (const id of fs.readdirSync(repoDir)) {
      const worktreeDir = path.join(repoDir, id);
      if (!isLiveDirectory(worktreeDir)) continue;
      const gitFile = path.join(worktreeDir, ".git");
      if (fs.existsSync(gitFile) && fs.statSync(gitFile).isFile()) {
        found.push(worktreeDir);
      }
    }
  }
  return found;
}

/**
 * One-time move from the old `~/.bitbucket-ai-companion` to the new
 * `~/.ai-dev-companion`, meant to run once at server startup before
 * anything else touches either path. A no-op unless the legacy dir exists
 * and the new one doesn't, so a second run — or an install that never had
 * a legacy dir — does nothing.
 *
 * Moving the directory does NOT touch a worktree checkout's own `.git`
 * file — that always points back at its main repo (`gitdir:
 * <main-repo>/.git/worktrees/<id>`), which never moves. What DOES go
 * stale is the *main repo's* own record of where this worktree lives
 * (`<main-repo>/.git/worktrees/<id>/gitdir`, which git writes as an
 * absolute path back to the worktree's `.git` file): it still names the
 * old, now-gone location. `git worktree repair`, run from inside each
 * moved worktree with its new path, rewrites that record to match. A
 * repair failure is logged and ignored rather than aborting the migration:
 * a broken worktree link is recoverable by hand (re-running `git worktree
 * repair`, or just recreating the job) and shouldn't strand the rest of
 * the user's state directory mid-move.
 *
 * Never throws: any unexpected failure (including the initial move itself)
 * is captured in `errors` instead, so a broken migration can never keep
 * the service from starting. Synchronous on purpose — see defaultGit()'s
 * comment — so server.ts can call it as a plain statement before building
 * FEATURES, with no async bootstrap wrapper needed.
 */
function migrateStateDir({ home = os.homedir(), git = defaultGit, log = () => {} } = {}) {
  const legacy = legacyStateDir(home);
  const current = stateDir(home);
  const result = { migrated: false, repaired: [], errors: [] };

  if (!fs.existsSync(legacy) || fs.existsSync(current)) return result;

  try {
    renameOrCopy(legacy, current);
    result.migrated = true;
  } catch (err) {
    result.errors.push(`Could not move ${legacy} to ${current}: ${err.message}`);
    return result;
  }

  for (const worktreeDir of findWorktreeDirs(current)) {
    try {
      git(["worktree", "repair", worktreeDir], worktreeDir);
      result.repaired.push(worktreeDir);
    } catch (err) {
      // execFileSync's own err.message is often just "Command failed: git
      // ..." with no detail; err.stderr (git's actual complaint) is what's
      // worth surfacing, when the runner set it.
      const detail = err.stderr ? String(err.stderr).trim() : err.message;
      const message = `git worktree repair failed for ${worktreeDir}: ${detail}`;
      log(message);
      result.errors.push(message);
    }
  }

  try {
    fs.mkdirSync(legacy, { recursive: true });
    fs.writeFileSync(path.join(legacy, "MOVED"), `${current}\n`);
  } catch (err) {
    result.errors.push(`Could not write ${path.join(legacy, "MOVED")}: ${err.message}`);
  }

  return result;
}

/**
 * Whether migrateStateDir should even run. Two independent reasons to
 * skip, both meant to keep a dev checkout from ever moving a *live*
 * install's state out from under it: `isStableInstall` is false for any
 * process not running from the stable install location (a dev checkout
 * started with `npm run dev`/`npm start` in a git worktree, say — see
 * core/updater.js's cannotApplyReason(), which server.ts reuses here), and
 * `AI_DEV_COMPANION_SKIP_MIGRATION=1` is an explicit operator opt-out
 * (e.g. someone deliberately inspecting a live install's state dir by
 * hand and not wanting it moved mid-inspection).
 */
function shouldMigrate({ isStableInstall, env = process.env } = {}) {
  if (env.AI_DEV_COMPANION_SKIP_MIGRATION === "1") return false;
  return !!isStableInstall;
}

module.exports = {
  STATE_DIR_NAME,
  LEGACY_STATE_DIR_NAME,
  stateDir,
  repoWorktreesRoot,
  legacyStateDir,
  migrateStateDir,
  shouldMigrate,
};
