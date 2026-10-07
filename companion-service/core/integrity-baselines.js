// The shared, persisted worktree-integrity baseline store.
//
// Before this module, resolve-conflict and address-review-comments each
// kept their own module-private `gitMetadataBaseline` Map of
// `{ gitDir, snapshot }`, taken right before a Claude run. A service
// restart (an auto-update, a crash) threw that Map away with everything
// else in memory — so a later `assertGitMetadataIntact` for a job that
// survived the restart (core/job-files.js reconciles a job's *status*,
// but the worktree itself is still sitting there) had nothing to compare
// against, and the check silently had nothing to do. This module gives
// both features one baseline store that also writes each snapshot to
// `<dir>/<id>.integrity.json` (via core/job-files.js's baselineFile/
// writeJsonAtomic), so a fresh process can rebuild its cache from disk
// instead of trusting the worktree unchecked.
//
// The in-memory cache always wins over disk once a snapshot has been
// taken in this process: `get()` only reads the file when the id isn't
// already cached, which in practice means "this process didn't take this
// baseline itself" (a restart, or a lookup for an id it never saw). That
// keeps the guarantee this exists to protect: nothing that can write to
// disk mid-run — including a resumed, unconfined Claude session, or the
// worktree's own confined one if it somehow reached outside its sandbox
// — can replace the snapshot this process took with its own hands.
//
// It lives in stateDir (via server.ts's `baselines.setDir(jobsDir(...))`)
// rather than anywhere under the worktree, because stateDir/jobs is
// outside both the sandbox's `filesystem.denyWrite` reach (which only
// covers `cwd`/`gitDir`, i.e. the worktree and its private git metadata)
// and the edit guard's write reach (which only allows the worktree
// itself). A baseline sitting next to the thing it's meant to detect
// tampering in would be tamperable by the exact same actor.
//
// Plain JS, not TypeScript, for the same reason as core/job-files.js and
// core/worktree-integrity.js: no build step needed to unit test it, and
// both features' TypeScript loads it with require + a hand-written type
// cast.
const fs = require("fs");
const { baselineFile, writeJsonAtomic } = require("./job-files.js");
const { snapshotWorktreeGitMetadata, worktreeGitMetadataChanged } = require("./worktree-integrity.js");

/** The exact keys a snapshot must have — see isValidEntry. Kept as a
 * sorted array so shape-checking isn't sensitive to key order. */
const SNAPSHOT_KEYS = ["commondir", "configWorktree", "gitdir", "gitlink"];

/** Whether `value` is a valid `{ gitDir, snapshot }` baseline entry: a
 * non-empty string `gitDir`, plus a `snapshot` object with exactly the
 * four keys snapshotWorktreeGitMetadata produces, each `string | null`.
 * Anything else (missing key, extra key, wrong type, not an object at
 * all) is untrusted and treated as if the baseline were simply missing —
 * see the module comment on why a malformed file must never be trusted
 * as "no change detected". */
function isValidEntry(value) {
  if (!value || typeof value !== "object") return false;
  if (typeof value.gitDir !== "string" || value.gitDir === "") return false;
  const snapshot = value.snapshot;
  if (!snapshot || typeof snapshot !== "object") return false;
  const keys = Object.keys(snapshot).sort();
  if (keys.length !== SNAPSHOT_KEYS.length || !keys.every((k, i) => k === SNAPSHOT_KEYS[i])) return false;
  return SNAPSHOT_KEYS.every((k) => typeof snapshot[k] === "string" || snapshot[k] === null);
}

/**
 * Creates a baseline store. `dir`, if set, is the directory (normally
 * `jobsDir(stateDir())`) baselines are persisted under; `null` means
 * memory-only (used by tests, and by any store before server.ts decides
 * whether this is the installed copy — see Decision 2 in the plan).
 */
function createBaselineStore({ dir = null } = {}) {
  /** jobId -> { gitDir, snapshot } for every baseline taken (or loaded
   * from disk) in this process. Wins over disk — see the module comment. */
  const cache = new Map();

  function setDir(newDir) {
    dir = newDir;
  }

  /**
   * Snapshots `{ worktreeDir, gitDir }` now, writes it to disk if `dir` is
   * set, and only then caches it under `jobId`. Throws if that write fails
   * (e.g. `dir`'s parent isn't writable): callers run this before any
   * Claude run, and a baseline that can't survive a restart shouldn't
   * silently let the job proceed as if it could (Decision 8). Written
   * before cached so a failed write leaves nothing behind: a cache-only
   * baseline would make this process trust a worktree that the next one
   * would have no record of.
   */
  function take(jobId, { worktreeDir, gitDir }) {
    const snapshot = snapshotWorktreeGitMetadata({ worktreeDir, gitDir });
    const entry = { gitDir, snapshot };
    if (dir !== null) {
      writeJsonAtomic(baselineFile(dir, jobId), entry);
    }
    cache.set(jobId, entry);
    return entry;
  }

  /**
   * Returns `{ gitDir, snapshot }` for `jobId`, or `null` if there is
   * none. Checks the cache first (see the module comment on why it
   * wins); only when the id isn't cached does it fall back to reading
   * `dir`'s file, and only when that file both parses and has the right
   * shape (isValidEntry) — an unreadable or malformed file is treated
   * exactly like a missing one, never thrown.
   */
  function get(jobId) {
    if (cache.has(jobId)) return cache.get(jobId);
    if (dir === null) return null;
    let raw;
    try {
      raw = fs.readFileSync(baselineFile(dir, jobId), "utf8");
    } catch {
      return null;
    }
    let parsed;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return null;
    }
    if (!isValidEntry(parsed)) return null;
    cache.set(jobId, parsed);
    return parsed;
  }

  /**
   * `"intact"` if `worktreeDir`'s current git metadata matches the
   * baseline for `jobId`; `"missing"` if there is no (valid) baseline for
   * `jobId` at all; `"changed"` otherwise — including when re-snapshotting
   * `worktreeDir` itself throws (e.g. one of the four paths became a
   * directory instead of a file). A read error is exactly the kind of
   * thing this check exists to notice, so it must never be mistaken for
   * "nothing changed".
   */
  function check(jobId, worktreeDir) {
    const entry = get(jobId);
    if (entry === null) return "missing";
    let after;
    try {
      after = snapshotWorktreeGitMetadata({ worktreeDir, gitDir: entry.gitDir });
    } catch {
      return "changed";
    }
    return worktreeGitMetadataChanged(entry.snapshot, after) ? "changed" : "intact";
  }

  /** Drops `jobId` from the cache and deletes its file, if any. A no-op,
   * not an error, when there's nothing to remove. */
  function remove(jobId) {
    cache.delete(jobId);
    if (dir !== null) {
      fs.rmSync(baselineFile(dir, jobId), { force: true });
    }
  }

  return { setDir, take, get, check, remove };
}

/** The process-wide store both worktree features share. */
const baselines = createBaselineStore();

module.exports = { createBaselineStore, baselines };
