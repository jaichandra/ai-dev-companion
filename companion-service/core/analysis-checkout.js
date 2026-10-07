// A read-only checkout of a repo's latest default branch for Analyze issue.
// It lives under the state dir (<stateDir>/repos/<PROJECT>/<repo>), never in
// the user's own folders: the analysis reads the current origin/<default>
// instead of whatever branch and half-finished edits the user's clone has, and
// leaves that clone, and every `<repo>.worktrees` folder beside it, alone.
//
// First use makes a local clone of the user's clone (object files are
// hard-linked, so it is quick and small), points its origin at the real
// remote and fetches. Later uses just fetch and move the detached checkout.
const fs = require("fs");
const path = require("path");
const { defaultGit } = require("./ticket-git.js");

const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
const FETCH_TIMEOUT_MS = 5 * 60 * 1000;

/** `<root>/<PROJECT>/<repo>` for a "PROJECT/repo" key, or null for anything else. */
function checkoutPath(root, repoKey) {
  const parts = String(repoKey || "").split("/");
  if (parts.length !== 2 || !parts.every((p) => NAME_RE.test(p) && p !== "." && p !== "..")) return null;
  return path.join(root, parts[0], parts[1]);
}

// One refresh of a checkout at a time: two analyses of the same repo must not
// fetch and check out into the same folder at once.
const inFlight = new Map();

function serialized(dir, work) {
  const previous = inFlight.get(dir) || Promise.resolve();
  const next = previous.catch(() => {}).then(work);
  inFlight.set(dir, next);
  const clear = () => {
    if (inFlight.get(dir) === next) inFlight.delete(dir);
  };
  next.then(clear, clear);
  return next;
}

async function originHeadOf(git, dir) {
  const head = await git(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], dir, { allowFailure: true });
  return head.code === 0 ? head.stdout.trim().replace(/^origin\//, "") : "";
}

async function hasRemoteBranch(git, dir, name) {
  if (!name) return false;
  const r = await git(["rev-parse", "--verify", "--quiet", `refs/remotes/origin/${name}`], dir, { allowFailure: true });
  return r.code === 0;
}

/**
 * The remote's default branch. A local clone of the user's clone inherits
 * that clone's *checked-out* branch as its origin/HEAD, so it is not trusted
 * until `remote set-head --auto` has asked the real remote. Failing that, the
 * user's own clone's origin/HEAD (set when it was cloned), then master/main.
 */
async function defaultBranchOf(git, dir, userClone, asked) {
  if (asked) {
    const name = await originHeadOf(git, dir);
    if (await hasRemoteBranch(git, dir, name)) return name;
  }
  const theirs = await originHeadOf(git, userClone);
  if (await hasRemoteBranch(git, dir, theirs)) return theirs;
  for (const candidate of ["master", "main"]) {
    if (await hasRemoteBranch(git, dir, candidate)) return candidate;
  }
  return null;
}

/**
 * Brings the checkout for `repoKey` up to date and returns
 * `{ dir, branch, fresh }`: `fresh` is false when the fetch failed (offline,
 * no access) and the checkout is as of the last time it worked. Throws when
 * there is no usable checkout at all. `git` is injectable for tests.
 */
function ensureAnalysisCheckout({ userClone, repoKey, root, git = defaultGit }) {
  const dir = checkoutPath(root, repoKey);
  if (!dir) return Promise.reject(new Error(`Not a repo key: ${String(repoKey).slice(0, 80)}`));
  return serialized(dir, async () => {
    const existed = fs.existsSync(path.join(dir, ".git"));
    if (!existed) {
      fs.rmSync(dir, { recursive: true, force: true });
      fs.mkdirSync(path.dirname(dir), { recursive: true });
      const remote = await git(["remote", "get-url", "origin"], userClone, { allowFailure: true });
      await git(["clone", "--quiet", "--no-checkout", userClone, dir], path.dirname(dir), { timeoutMs: FETCH_TIMEOUT_MS });
      const url = remote.code === 0 ? remote.stdout.trim() : "";
      if (url) await git(["remote", "set-url", "origin", url], dir);
    }

    let fresh = true;
    try {
      await git(["fetch", "--quiet", "--prune", "origin"], dir, { timeoutMs: FETCH_TIMEOUT_MS });
    } catch (err) {
      fresh = false;
      if (!existed) {
        // A local clone of the user's clone still has its origin/* refs; usable, just as old as that clone.
        console.warn(`[analysis-checkout] first fetch of ${repoKey} failed: ${err.message}`);
      } else {
        console.warn(`[analysis-checkout] fetch of ${repoKey} failed, using the last copy: ${err.message}`);
      }
    }

    let asked = false;
    if (fresh) {
      const r = await git(["remote", "set-head", "origin", "--auto"], dir, { allowFailure: true, timeoutMs: FETCH_TIMEOUT_MS });
      asked = r.code === 0;
    }
    const branch = await defaultBranchOf(git, dir, userClone, asked);
    if (!branch) throw new Error(`No origin/master or origin/main in the checkout of ${repoKey}.`);
    await git(["checkout", "--quiet", "--force", "--detach", `origin/${branch}`], dir, { timeoutMs: FETCH_TIMEOUT_MS });
    return { dir, branch, fresh };
  });
}

module.exports = { checkoutPath, ensureAnalysisCheckout };
