// A `scopeKey` ties a job to the browser page it belongs to (a Bitbucket
// PR, a Jira issue, a pre-deployment-stats Jenkins pipeline) — Task 6's
// extension side reads it back via GET /jobs/lookup so a page can show
// "there's already a pending job for this PR/issue" instead of a
// duplicate. Plain JS, not TypeScript (like core/prereqs.js): server.ts
// and core/jobs.ts require it directly with a hand-written type cast, and
// lookupJob works on plain job objects so it never has to import
// core/jobs.ts's TS types.
const { isSafeRepoSegment, gitRemoteRules } = require("./prereqs.js");
const packs = require("./packs.js");

/** A positive integer, given either as a real number or as a plain digit
 * string — the two shapes an MCP tool's `prId` argument or an extension's
 * JSON body might send it as. `0`, negatives, non-integers ("1.5") and a
 * string with anything but digits ("12a") are all rejected. */
function isPositiveInt(value) {
  if (typeof value === "number") return Number.isInteger(value) && value > 0;
  if (typeof value === "string") return /^\d+$/.test(value) && Number(value) > 0;
  return false;
}

/** `<host>:<project>/<repo>#<n>` for resolve-conflict and address-review-comments, whose payload is
 * `{project, repo, prId}`: `bitbucket:PROJECT/repo#n` (project upper-cased, repo lower-cased, as Bitbucket
 * Server itself cases them) or `github:owner/repo#n` (both lower-cased), by this distribution's git provider
 * (see core/prereqs.js's gitRemoteRules). `null` for anything that doesn't look like a real
 * project/repo/prId. */
function prScopeKey(payload) {
  if (!payload || typeof payload !== "object") return null;
  const { project, repo, prId } = payload;
  if (!isSafeRepoSegment(project) || !isSafeRepoSegment(repo) || !isPositiveInt(prId)) return null;
  return gitRemoteRules().prKey(project, repo, Number(prId));
}

/** Jira issue keys are `<PROJECT KEY><ticket number>`, e.g. "PROJ-1" — no
 * internal dashes in the project part, so this doesn't reuse
 * isSafeRepoSegment (which would also accept things like "proj.1"). */
const ISSUE_KEY_RE = /^([A-Za-z][A-Za-z0-9]*)-([1-9]\d*)$/;

/** `jira:KEY-n` for analyze-issue, whose payload is `{issueKey}` —
 * upper-cased to match how the rest of the service (e.g.
 * features/analyze-issue's plan.parseIssueKey) treats issue keys. `null`
 * for a payload with no ticket number ("PROJ") or anything else malformed. */
function jiraScopeKey(payload) {
  if (!payload || typeof payload !== "object" || typeof payload.issueKey !== "string") return null;
  const match = ISSUE_KEY_RE.exec(payload.issueKey);
  if (!match) return null;
  return `jira:${match[1].toUpperCase()}-${match[2]}`;
}

/**
 * The scopeKey a job for `featureId` belongs under, given the payload its
 * start() call was made with — or `null` if `featureId` doesn't tag jobs
 * this way, or the payload doesn't look like a real one for it. Each feature
 * declares its own rule (`scopeKey` in its features/<id>/feature.js: one of the
 * named kinds below, or a function). Used by server.ts's startFeatureJob to
 * stamp every job, and by an MCP tool to build the same key it'll later look
 * the job up by.
 */
function scopeKeyFor(featureId, payload) {
  return packs.scopeKeyFor(featureId, payload, { pr: prScopeKey, "bitbucket-pr": prScopeKey, "jira-issue": jiraScopeKey });
}

/** A positive integer written as a bare digit string — used for the `n` in
 * `#n`, unlike isPositiveInt above (which also accepts a real number),
 * because a scopeKey is always a string that was already split apart. */
function isPositiveIntString(value) {
  return /^[1-9]\d*$/.test(value);
}

const MAX_SCOPE_KEY_LENGTH = 300;

/**
 * Whether `s` is a well-formed scopeKey — checked with anchored regexes so
 * nothing but exactly one of the four shapes below can pass, however it's
 * spelled: `bitbucket:<SEG>/<seg>#<n>` or `github:<seg>/<seg>#<n>`, `jira:<KEY>-<n>`,
 * `jenkins:<helm job>`, or (reserved for Phase 5's per-build
 * features) `jenkins:<seg>#<n>`. Every path-like segment is additionally
 * checked with isSafeRepoSegment, and every `n` must be a positive
 * integer — so this rejects, for instance, a trailing newline (JS's `$`
 * only matches the true end of the string, not before one), SQL-looking
 * junk in place of a build number, or anything over 300 characters (GET
 * /jobs/lookup's query-string input has no other length limit). Used by
 * both GET /jobs/lookup (reject a malformed scopeKey with 400 rather than
 * querying with it) and — implicitly, since scopeKeyFor only ever
 * produces valid keys — by lookupJob's callers.
 */
function isValidScopeKey(s) {
  if (typeof s !== "string" || s.length > MAX_SCOPE_KEY_LENGTH) return false;

  const helm = /^jenkins:([^#]+)$/.exec(s);
  if (helm) return isSafeRepoSegment(helm[1]);

  const pr = /^(?:bitbucket|github):([^/]+)\/([^#]+)#([^#]+)$/.exec(s);
  if (pr) {
    const [, project, repo, n] = pr;
    return isSafeRepoSegment(project) && isSafeRepoSegment(repo) && isPositiveIntString(n);
  }

  const jira = /^jira:([^-]+)-([^-]+)$/.exec(s);
  if (jira) {
    const [, key, n] = jira;
    return isSafeRepoSegment(key) && isPositiveIntString(n);
  }

  const jenkins = /^jenkins:([^#]+)#([^#]+)$/.exec(s);
  if (jenkins) {
    const [, job, n] = jenkins;
    return isSafeRepoSegment(job) && isPositiveIntString(n);
  }

  return false;
}

/**
 * The newest (by `createdAt`) job in `jobs` that belongs to `scopeKey` and
 * `featureId`, was started through an MCP tool or a background watcher
 * (`startedVia` "mcp" or "watcher" — an extension-started job never needs
 * looking up this way, since the page that started it already has its
 * id), and hasn't already been
 * resolved (`approved`/`rejected`) — or `null` if none matches. Takes
 * plain job objects rather than importing core/jobs.ts's `Job` type, so
 * this stays a dependency-free JS module; core/jobs.ts's
 * `JobStore.lookup` is the typed wrapper that passes it `[...this.jobs.values()]`.
 */
function lookupJob(jobs, scopeKey, featureId) {
  let newest = null;
  for (const job of jobs) {
    if (job.scopeKey !== scopeKey || job.featureId !== featureId) continue;
    if (job.startedVia !== "mcp" && job.startedVia !== "watcher") continue;
    if (job.status === "approved" || job.status === "rejected") continue;
    if (!newest || job.createdAt > newest.createdAt) newest = job;
  }
  return newest;
}

module.exports = {
  scopeKeyFor,
  isValidScopeKey,
  lookupJob,
};
