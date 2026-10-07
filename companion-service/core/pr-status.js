// What the extension asks about the pull request on the page: is it open, does it conflict, which branches,
// is it from a fork. The extension can't ask the git host itself (GitHub's page cookies don't authenticate its
// API, and Bitbucket's REST shape is its own business), so it asks the companion, which reads the PR through the
// configured git provider and answers in one host-neutral shape. Answers are cached briefly — the extension
// re-checks the page every couple of seconds, which would otherwise be a stream of API calls — and calls
// for the same PR that arrive together share one lookup.
const { isSafeRepoSegment } = require("./prereqs.js");

const DEFAULT_TTL_MS = 20_000;
const ERROR_TTL_MS = 5_000;
const MAX_ENTRIES = 200;

/** A request that isn't about a well-formed pull request (the route answers 400). */
class PrStatusRequestError extends Error {}

function sameName(a, b) {
  return typeof a === "string" && typeof b === "string" && a.toLowerCase() === b.toLowerCase();
}

/** True when the PR's source repository differs from its target, false when it is the same, null when unknown. */
function isFork(pr) {
  const from = pr && pr.fromRepo;
  const to = pr && pr.toRepo;
  if (!from || !to || !from.projectKey || !from.slug || !to.projectKey || !to.slug) return null;
  return !(sameName(from.projectKey, to.projectKey) && sameName(from.slug, to.slug));
}

const num = (v) => (typeof v === "number" && Number.isFinite(v) ? v : null);

/**
 * `getGit()` returns the git provider (core/providers.ts GitHost); `now` is injectable for tests.
 * `get(auth, project, repo, prId)` resolves to
 *   { state, conflicted, isFork, fromBranch, toBranch, title, prUrl, commentCount, openTaskCount }
 * where every field the host can't say is null.
 */
function createPrStatus({ getGit, now = Date.now, ttlMs = DEFAULT_TTL_MS }) {
  const cache = new Map(); // key -> { at, ttl, promise }

  async function lookup(git, auth, project, repo, prId) {
    const pr = await git.getPullRequest(auth, project, repo, prId);
    if (!pr) throw new Error(`The git host didn't return pull request ${project}/${repo}#${prId}.`);
    let conflicted = null;
    // A merged or declined PR has nothing left to conflict, and the check costs a call.
    if (pr.state === "OPEN" && git.capabilities && git.capabilities.has("mergeStatus")) {
      try {
        conflicted = (await git.getMergeStatus(auth, project, repo, prId)).conflicted;
      } catch {
        conflicted = null;
      }
    }
    return {
      state: typeof pr.state === "string" ? pr.state : null,
      conflicted: typeof conflicted === "boolean" ? conflicted : null,
      isFork: isFork(pr),
      fromBranch: typeof pr.fromBranch === "string" ? pr.fromBranch : null,
      toBranch: typeof pr.toBranch === "string" ? pr.toBranch : null,
      title: typeof pr.title === "string" ? pr.title : null,
      prUrl: git.prUrl(project, repo, prId),
      commentCount: num(pr.commentCount),
      openTaskCount: num(pr.openTaskCount),
    };
  }

  return {
    async get(auth, project, repo, prId) {
      if (!isSafeRepoSegment(project) || !isSafeRepoSegment(repo)) throw new PrStatusRequestError("project and repo must be plain names");
      if (!Number.isInteger(prId) || prId < 1) throw new PrStatusRequestError("prId must be a positive whole number");
      const key = `${project.toLowerCase()}/${repo.toLowerCase()}#${prId}`;
      const hit = cache.get(key);
      if (hit && now() - hit.at < hit.ttl) return hit.promise;
      const promise = lookup(getGit(), auth, project, repo, prId);
      const entry = { at: now(), ttl: ttlMs, promise };
      cache.set(key, entry);
      if (cache.size > MAX_ENTRIES) cache.delete(cache.keys().next().value);
      // A failure is remembered only briefly, so a fixed token or a PR that appears later is picked up soon.
      promise.catch(() => {
        entry.ttl = ERROR_TTL_MS;
      });
      return promise;
    },
    clear: () => cache.clear(),
  };
}

module.exports = { createPrStatus, PrStatusRequestError, isFork };
