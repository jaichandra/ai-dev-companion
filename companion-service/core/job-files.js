// Restart-resilient review jobs: the on-disk layer under core/jobs.ts's
// JobStore. That store is deliberately in-memory only today, so any
// service restart mid-job (an auto-update, a crash, a laptop sleep/wake
// that kills the process) silently drops whatever the user was reviewing —
// including a job stuck "running" with no way to tell if it actually
// finished. This module is the plain-JS disk half of fixing that: it
// saves/loads one JSON file per job, reconciling anything caught mid-flight
// to a clear "failed" state instead of just vanishing, and pruning what's
// old enough that nobody's coming back for it. core/jobs.ts wires this in;
// a later module reuses baselineFile/writeJsonAtomic for a companion
// integrity-baseline store.
//
// Only features a caller explicitly opts in (persistAction's `featureIds`)
// get their jobs written to disk at all — a feature's `data`/`result`
// payload can hold things that don't mean anything after a restart (a live
// Claude session id, an assumption that the process that created a
// worktree is still around) unless it's been reviewed for that, so
// unreviewed features simply keep today's in-memory-only behavior.
//
// Loading is deliberately paranoid, because these files are read back
// blind at startup: every id is checked against crypto.randomUUID's own
// shape *before* it's used to build a path, so a corrupted or tampered
// `<id>.json` name can never walk a read/write outside the jobs directory;
// and every `data.worktree.dir` is checked to still be strictly inside the
// worktree root before anything trusts it, so a stale or tampered job file
// can't point a later step (e.g. "is this worktree still here?") at some
// unrelated path on disk.
//
// Plain JS, not TypeScript, for the same reason as core/paths.js: no build
// step needed to unit test it, and core/jobs.ts's TypeScript requires it
// directly.
const fs = require("fs");
const path = require("path");
const packs = require("./packs.js");

/** How long a terminal job's files are kept before loadJobs prunes them —
 * long enough to look back at a job from a couple of weeks ago, short
 * enough that a machine that's rarely restarted doesn't accumulate them
 * forever. */
const PRUNE_AFTER_MS = 14 * 24 * 60 * 60 * 1000;

/** The jobs of `featureIds` that are written to disk. A feature opts out with
 * `persist: false` in its features/<id>/feature.js — review-in-editor restores
 * from its own session records (a second copy would race it), a
 * pre-deployment-stats report can be large and is meant to be re-run, a
 * ticket-workspace scan is quick to redo (and its folders and sessions must
 * only ever be ones this process found, never read back from a file), and a
 * digest is a snapshot, stale by the next morning. */
function featureIdsToPersist(featureIds) {
  const skip = packs.notPersistedIds();
  return featureIds.filter((id) => !skip.includes(id));
}

/** A job nobody acted on for `maxAgeMs` — only awaiting-approval and failed
 * jobs can be stale; a running one is never swept. */
function isStale(job, now, maxAgeMs = PRUNE_AFTER_MS) {
  if (job.status !== "awaiting-approval" && job.status !== "failed") return false;
  return Number.isFinite(job.updatedAt) && now - job.updatedAt > maxAgeMs;
}

/** Statuses that only make sense while this process is the one driving
 * them — anything left in one of these after a restart was cut off
 * mid-step, not actually still running. "pending-start" isn't here: it
 * has no worktree and isn't driven by this process at all until a
 * browser Start click runs it, so it just survives a restart as-is. */
const INTERRUPTED_STATUSES = ["running", "approving", "rejecting"];

const RESTART_MESSAGE =
  "The service restarted while this job was in progress (service restarted — click Refresh diff).";

/** For an interrupted job that never got as far as creating a worktree:
 * there's nothing for Refresh diff to pick up, so telling the user to
 * click it would only lead to a refusal. */
const RESTART_MESSAGE_NO_WORKTREE =
  "The service restarted before this job had anything to review (service restarted — start it again).";

/** The exact shape of `crypto.randomUUID()`'s output (8-4-4-4-12 lowercase
 * hex). Every job id is checked against this before it's used to build a
 * filename — see the module comment. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

function jobsDir(stateDir) {
  return path.join(stateDir, "jobs");
}

function assertJobId(id) {
  if (!UUID_RE.test(id)) throw new Error(`Not a job id: ${id}`);
}

/** `<dir>/<id>.json` — throws on a non-UUID id, since it's about to become
 * a filename (path safety, not just validation). */
function jobFile(dir, id) {
  assertJobId(id);
  return path.join(dir, `${id}.json`);
}

/** `<dir>/<id>.integrity.json` — where a companion integrity-baseline store
 * keeps one job's snapshot, next to its job file. Same id check as
 * jobFile. */
function baselineFile(dir, id) {
  assertJobId(id);
  return path.join(dir, `${id}.integrity.json`);
}

/**
 * Writes `value` as JSON to `file`, atomically: `mkdir -p` its directory
 * (0700), write to a per-process tmp file (0600) next to it, then rename
 * that over the real path. A reader of `file` therefore never sees a
 * half-written file, and a crash mid-write leaves at most a stray
 * `<file>.<pid>.tmp` behind — never a corrupt `<id>.json`.
 */
function writeJsonAtomic(file, value) {
  const dir = path.dirname(file);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(value, null, 2), { mode: 0o600 });
  fs.renameSync(tmp, file);
}

function saveJob(dir, job) {
  writeJsonAtomic(jobFile(dir, job.id), job);
}

/** Removes both of `id`'s files (job + baseline), if present; a no-op,
 * not an error, when neither exists. */
function removeJob(dir, id) {
  fs.rmSync(jobFile(dir, id), { force: true });
  fs.rmSync(baselineFile(dir, id), { force: true });
}

/**
 * Whether — and how — a job should be persisted: `"skip"` if its feature
 * hasn't opted in (`featureIds` doesn't include `job.featureId`, see the
 * module comment); `"remove"` once it's reached a terminal state a restart
 * can't affect (`approved` or `rejected` — nothing left worth restoring);
 * `"save"` otherwise.
 */
function persistAction(job, featureIds) {
  if (!featureIds.includes(job.featureId)) return "skip";
  if (job.status === "approved" || job.status === "rejected") return "remove";
  return "save";
}

/** Whether `target` resolves to strictly inside `root` — not equal to it,
 * and not escaping it via `..`. Used to check a job's recorded
 * `data.worktree.dir` is still trustworthy before anything reads it. A
 * missing/non-string `root` (the caller forgot to pass `worktreeRoot`) or a
 * non-string `target` is treated as "not inside" rather than thrown on —
 * `path.resolve` only accepts strings, and a job file is untrusted input
 * that must never be able to crash the whole load by putting something
 * odd in `data.worktree.dir`. */
function isStrictlyInside(root, target) {
  if (typeof root !== "string" || typeof target !== "string") return false;
  const rel = path.relative(path.resolve(root), path.resolve(target));
  return rel !== "" && !rel.startsWith("..") && !path.isAbsolute(rel);
}

/** Whether `dir` is exactly `<worktreeRoot>/<repo-slug>/<id>`, the layout
 * core/worktree.ts's addWorktree creates for job `id`. Stricter than
 * isStrictlyInside on purpose: discard may `rm -rf` this path, so "somewhere
 * under the root" isn't enough — a dir directly under the root is a whole
 * repo's worktrees, and one named for another id is another job's. */
function isJobWorktreeDir(worktreeRoot, dir, id) {
  if (typeof dir !== "string" || dir === "") return false;
  if (!isStrictlyInside(worktreeRoot, dir)) return false;
  const resolved = path.resolve(dir);
  return path.basename(resolved) === id && path.dirname(path.dirname(resolved)) === path.resolve(worktreeRoot);
}

/**
 * Loads every persisted job from `dir`, reconciling interrupted statuses
 * and pruning stale ones as it goes: `{ jobs, reconciled, pruned, prunedJobs, dropped }`
 * (`jobs` holds job objects; `reconciled`, `pruned` and `dropped` are lists of job ids; `prunedJobs` holds the pruned job objects). Never throws — a missing
 * `dir`, an unreadable or malformed file, or an unsafe path are all just
 * skipped/dropped, so one bad file can't keep the rest of a user's jobs
 * from loading at startup.
 *
 * - Reads every `*.json` in `dir` that isn't `*.integrity.json` or `*.tmp`.
 * - A parse error, an `id` that isn't a UUID, or an `id` that doesn't match
 *   the file it came from: the file is removed, its (file-name) id is
 *   added to `dropped`, and it's logged.
 * - An `updatedAt` that isn't a finite number: same treatment (removed,
 *   added to `dropped`, logged) — otherwise a job like this would never
 *   age out of the stale check below (`now - NaN` is always `NaN`, and
 *   `NaN > maxAgeMs` is always false).
 * - A `data.worktree` whose `dir` isn't exactly `<worktreeRoot>/<repo>/<id>`
 *   (isJobWorktreeDir) — including an empty or missing `dir`, a non-string
 *   one, or a missing/non-string `worktreeRoot` — or a `data.claudeSession`
 *   whose `cwd` isn't that same `dir`: the job (and its baseline) is
 *   removed, added to `dropped`, and logged. When there's no worktree,
 *   sessionCwdOk decides whether a Claude session is allowed.
 * - A job older than `maxAgeMs` (by `updatedAt`): removed via removeJob,
 *   added to `pruned`, and the job object added to `prunedJobs`. If its
 *   worktree dir still exists on disk, that's logged too — removing the
 *   worktree is the caller's job (server.ts does it for `prunedJobs`).
 * - A job in `INTERRUPTED_STATUSES`: rewritten to `failed` with
 *   `RESTART_MESSAGE` (or `RESTART_MESSAGE_NO_WORKTREE` when it has no
 *   `data.worktree`), no `progress`, and a fresh `updatedAt`; saved back
 *   to disk and added to `reconciled`. If that save itself fails (e.g.
 *   EACCES, ENOSPC), the failure is only logged — the in-memory job still
 *   comes back `failed` and still counts as `reconciled`, since only the
 *   on-disk copy, not the caller's view of the job, is allowed to be lost.
 * - Finally, any `<id>.integrity.json` left with no matching `<id>.json`
 *   is removed (an orphaned baseline from a job that's gone).
 */
function loadJobs(
  dir,
  { now = Date.now(), worktreeRoot, maxAgeMs = PRUNE_AFTER_MS, log = () => {}, sessionCwdOk = () => false } = {},
) {
  const result = { jobs: [], reconciled: [], pruned: [], prunedJobs: [], dropped: [] };

  let names;
  try {
    names = fs.readdirSync(dir);
  } catch {
    return result;
  }

  for (const name of names) {
    // A tmp file from writeJsonAtomic is named `<id>.json.<pid>.tmp`, so it
    // never ends in ".json" and is already skipped by the first check below
    // — this just spells that out, since a bad tmp file left over from a
    // crashed write must never be read as a job.
    if (!name.endsWith(".json") || name.endsWith(".integrity.json")) continue;
    const fileId = name.slice(0, -".json".length);
    const file = path.join(dir, name);

    let job;
    try {
      job = JSON.parse(fs.readFileSync(file, "utf8"));
    } catch (err) {
      log(`[jobs] dropping ${name}: ${err.message}`);
      fs.rmSync(file, { force: true });
      result.dropped.push(fileId);
      continue;
    }

    if (!UUID_RE.test(String(job.id)) || job.id !== fileId) {
      log(`[jobs] dropping ${name}: id is missing, invalid, or doesn't match the file name`);
      fs.rmSync(file, { force: true });
      result.dropped.push(fileId);
      continue;
    }

    const id = job.id;

    if (!Number.isFinite(job.updatedAt)) {
      log(`[jobs] dropping ${name}: updatedAt is missing or not a number`);
      fs.rmSync(file, { force: true });
      result.dropped.push(id);
      continue;
    }

    const worktree = job.data ? job.data.worktree : undefined;
    const worktreeDir = worktree && typeof worktree === "object" ? worktree.dir : undefined;

    // Any `data.worktree` at all must name exactly this job's own worktree
    // (isJobWorktreeDir) — an empty or missing `dir` included, since a job
    // that has a worktree is one discard will try to remove.
    if (worktree !== undefined && !isJobWorktreeDir(worktreeRoot, worktreeDir, id)) {
      log(`[jobs] dropping job ${id}: its worktree (${String(worktreeDir)}) isn't ${worktreeRoot}/<repo>/${id}`);
      removeJob(dir, id);
      result.dropped.push(id);
      continue;
    }

    // A recorded Claude session is resumed in its `cwd` (Continue in Claude
    // Code). With a worktree it must be exactly that worktree; without one
    // (analyze-issue) it must be a directory this service would have used —
    // sessionCwdOk decides, and the default refuses everything.
    const claudeSession = job.data ? job.data.claudeSession : undefined;
    if (claudeSession !== undefined) {
      const sessionCwd =
        claudeSession && typeof claudeSession === "object" ? claudeSession.cwd : undefined;
      const ok =
        worktree !== undefined
          ? sessionCwd === worktreeDir
          : typeof sessionCwd === "string" && sessionCwdOk(sessionCwd);
      if (!ok) {
        log(`[jobs] dropping job ${id}: its Claude session's cwd isn't an allowed directory`);
        removeJob(dir, id);
        result.dropped.push(id);
        continue;
      }
    }

    if (now - job.updatedAt > maxAgeMs) {
      removeJob(dir, id);
      result.pruned.push(id);
      result.prunedJobs.push(job);
      if (typeof worktreeDir === "string" && fs.existsSync(worktreeDir)) {
        log(`[jobs] pruned stale job ${id}; its worktree is left at ${worktreeDir}`);
      }
      continue;
    }

    if (INTERRUPTED_STATUSES.includes(job.status)) {
      job.status = "failed";
      job.error = worktree === undefined ? RESTART_MESSAGE_NO_WORKTREE : RESTART_MESSAGE;
      delete job.progress;
      job.updatedAt = now;
      // The in-memory reconciliation (marking it failed) must survive even
      // if writing it back to disk doesn't — e.g. EACCES, ENOSPC. Losing
      // the on-disk update just means a *second* restart would reconcile
      // it again from its old on-disk status; losing the in-memory one
      // would mean the service reports a job as still "running" forever.
      try {
        saveJob(dir, job);
      } catch (err) {
        log(`[jobs] could not save reconciled job ${id}: ${err.message}`);
      }
      result.reconciled.push(id);
    }

    result.jobs.push(job);
  }

  for (const name of names) {
    if (!name.endsWith(".integrity.json")) continue;
    const id = name.slice(0, -".integrity.json".length);
    if (!fs.existsSync(path.join(dir, `${id}.json`))) {
      fs.rmSync(path.join(dir, name), { force: true });
    }
  }

  return result;
}

module.exports = {
  PRUNE_AFTER_MS,
  INTERRUPTED_STATUSES,
  RESTART_MESSAGE,
  RESTART_MESSAGE_NO_WORKTREE,
  jobsDir,
  jobFile,
  baselineFile,
  writeJsonAtomic,
  saveJob,
  removeJob,
  persistAction,
  featureIdsToPersist,
  isStale,
  loadJobs,
};
