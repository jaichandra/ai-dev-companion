// Turns raw Bitbucket Server/DC REST JSON into stable internal shapes.
// Task 5 (companion-service/core/bitbucket.ts, not written yet) is the only
// caller that ever sees a real Bitbucket response — everything here is pure
// and defensive so a Bitbucket upgrade that renames/removes/adds a field
// can't silently break review-comment collection (Task 6) or, worse, throw
// and take the request down. Every field is read defensively: missing ->
// a documented default, unrecognized enum value -> "unknown", never throw.
//
// Plain JS (not TypeScript) for the same reason as core/paths.js and
// core/worktree-integrity.js: no build step needed to unit test it.
//
// fixtures/bitbucket/docs-8.x/*.json are hand-modeled on Atlassian's
// *documented* response shapes (see fixtures/bitbucket/README.md and this
// task's global-constraints.md) — not a real capture. A real capture goes
// in a sibling fixtures/bitbucket/real-<version>/ directory, tested
// automatically by core/bitbucket-normalize.test.js's fixture discovery.

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * The raw REST keys each normalizer reads, keyed by raw-object type. This
 * is the single source of truth: Task 5's live contract check compares a
 * real Bitbucket response's keys against these lists before merge, instead
 * of a second, hand-maintained key list drifting out of sync with this
 * file. Nested reads use a dotted path (e.g. "fromRef.displayId") rather
 * than a separate entry per nested object type.
 */
const EXPECTED_KEYS = {
  pullRequest: [
    "id",
    "version",
    "title",
    "state",
    "fromRef.displayId",
    "fromRef.latestCommit",
    "fromRef.repository.slug",
    "fromRef.repository.project.key",
    "toRef.displayId",
    "toRef.latestCommit",
    "toRef.repository.slug",
    "toRef.repository.project.key",
    "author.user.slug",
  ],
  activity: ["action", "comment", "commentAnchor"],
  comment: [
    "id",
    "version",
    "text",
    "author.slug",
    "author.displayName",
    "author.name",
    "createdDate",
    "state",
    "severity",
    "threadResolved",
    "comments",
  ],
  commentAnchor: ["path", "line", "lineType", "fileType", "diffType", "orphaned"],
  applicationProperties: ["version"],
};

/**
 * Dotted paths, keyed the same way as EXPECTED_KEYS, that a normalizer
 * reads but that a real Bitbucket Server/DC instance may legitimately not
 * return — confirmed on 9.4.16, whose `GET .../pull-requests/{id}` has no
 * `properties` key at all (only the *list* endpoint's
 * `?withProperties=true` returns per-PR `openTaskCount`/`commentCount`;
 * there's no single-PR equivalent). Kept apart from EXPECTED_KEYS so
 * doctor's live contract check can report a missing optional key as
 * informational rather than as a broken contract — see doctor.js's
 * `runBitbucketContractCheck`. normalizePullRequest already treats these
 * as optional (see its own doc comment); this list only affects the
 * contract check's reporting, not normalization.
 */
const OPTIONAL_KEYS = {
  pullRequest: ["properties.openTaskCount", "properties.commentCount"],
};

/** A ref's `repository` as `{projectKey, slug}` — each null when missing
 * or not a string. Kept even though the PR endpoint is already scoped to
 * one repo: a fork PR's `fromRef` lives in a DIFFERENT repository, and
 * address-review-comments must refuse it rather than push a same-named
 * branch in the wrong repo (see its plan.js's pullRequestRepoError). */
function normalizeRefRepo(ref) {
  const repo = isPlainObject(ref.repository) ? ref.repository : {};
  const project = isPlainObject(repo.project) ? repo.project : {};
  return {
    projectKey: typeof project.key === "string" ? project.key : null,
    slug: typeof repo.slug === "string" ? repo.slug : null,
  };
}

/**
 * `raw` is a `GET .../pull-requests/{id}` response. Returns
 * `{id, version, title, state, fromBranch, fromSha, fromRepo, toBranch,
 * toSha, toRepo, authorSlug, openTaskCount?, commentCount?}` (fromRepo/
 * toRepo are normalizeRefRepo's `{projectKey, slug}`), or `null` if `raw` isn't an
 * object. `openTaskCount`/`commentCount` are omitted (not even `null`) when
 * `raw.properties` doesn't carry them — Bitbucket only started publishing
 * per-PR task/comment counts in `properties` at some version, and callers
 * should be able to tell "we don't know" apart from "it's zero".
 */
function normalizePullRequest(raw) {
  if (!isPlainObject(raw)) return null;

  const fromRef = isPlainObject(raw.fromRef) ? raw.fromRef : {};
  const toRef = isPlainObject(raw.toRef) ? raw.toRef : {};
  const authorUser =
    isPlainObject(raw.author) && isPlainObject(raw.author.user) ? raw.author.user : {};
  const properties = isPlainObject(raw.properties) ? raw.properties : {};

  const result = {
    id: typeof raw.id === "number" ? raw.id : null,
    version: typeof raw.version === "number" ? raw.version : null,
    title: typeof raw.title === "string" ? raw.title : null,
    state: typeof raw.state === "string" ? raw.state : null,
    fromBranch: typeof fromRef.displayId === "string" ? fromRef.displayId : null,
    fromSha: typeof fromRef.latestCommit === "string" ? fromRef.latestCommit : null,
    fromRepo: normalizeRefRepo(fromRef),
    toBranch: typeof toRef.displayId === "string" ? toRef.displayId : null,
    toSha: typeof toRef.latestCommit === "string" ? toRef.latestCommit : null,
    toRepo: normalizeRefRepo(toRef),
    authorSlug: typeof authorUser.slug === "string" ? authorUser.slug : null,
  };
  if (typeof properties.openTaskCount === "number") result.openTaskCount = properties.openTaskCount;
  if (typeof properties.commentCount === "number") result.commentCount = properties.commentCount;
  return result;
}

const KNOWN_SEVERITIES = new Set(["NORMAL", "BLOCKER"]);
const KNOWN_STATES = new Set(["OPEN", "RESOLVED", "PENDING"]);
const KNOWN_LINE_TYPES = new Set(["ADDED", "REMOVED", "CONTEXT"]);
const KNOWN_FILE_TYPES = new Set(["FROM", "TO"]);
const KNOWN_DIFF_TYPES = new Set(["EFFECTIVE", "COMMIT", "RANGE"]);

/** A comment with no `severity` at all is Bitbucket's ordinary case (the
 * field is only really interesting once a comment has been flagged as a
 * BLOCKER task) — default to "NORMAL", the neutral value. A `severity`
 * that IS present but isn't one Bitbucket documents maps to "unknown", per
 * this module's blanket "unrecognized enum value -> unknown" rule. */
function normalizeSeverity(raw) {
  if (raw === undefined || raw === null) return "NORMAL";
  return KNOWN_SEVERITIES.has(raw) ? raw : "unknown";
}

/** Missing `state` defaults to "OPEN" — the conservative choice: an
 * unresolved/undetermined comment should keep showing up as needing
 * attention rather than silently disappearing from an "open comments"
 * view. An unrecognized value maps to "unknown". */
function normalizeState(raw) {
  if (raw === undefined || raw === null) return "OPEN";
  return KNOWN_STATES.has(raw) ? raw : "unknown";
}

/** `threadResolved` genuinely has no safe default to invent — unlike
 * severity/state, "we don't know" is a real, distinct third value here, so
 * a missing/non-boolean raw value stays `null` rather than being coerced
 * to `true` or `false`. */
function normalizeThreadResolved(raw) {
  return typeof raw === "boolean" ? raw : null;
}

/** `rawAnchor` is a `commentAnchor` object from a PR activity, or
 * undefined/null when the activity carries none (an ordinary top-level PR
 * comment, or a reply — replies never have their own anchor). Returns
 * `null` in that case; never an anchor-shaped object with fields guessed. */
function normalizeAnchor(rawAnchor) {
  if (!isPlainObject(rawAnchor)) return null;
  return {
    path: typeof rawAnchor.path === "string" ? rawAnchor.path : null,
    line: typeof rawAnchor.line === "number" ? rawAnchor.line : null,
    lineType: KNOWN_LINE_TYPES.has(rawAnchor.lineType) ? rawAnchor.lineType : "unknown",
    fileType: KNOWN_FILE_TYPES.has(rawAnchor.fileType) ? rawAnchor.fileType : "unknown",
    diffType: KNOWN_DIFF_TYPES.has(rawAnchor.diffType) ? rawAnchor.diffType : "unknown",
    orphaned: typeof rawAnchor.orphaned === "boolean" ? rawAnchor.orphaned : false,
  };
}

/** Normalizes one raw `comment` object (from an activity, or from a
 * parent's `comments[]`) into a ReviewComment. `rawAnchor` is only ever
 * passed for the top-level call from normalizeActivities — a reply's own
 * `comments[]` entries are recursed into with no anchor, since Bitbucket
 * never anchors a reply independently of its parent. Returns `null` for a
 * non-object `raw` (defensive; normalizeActivities already skips these,
 * but a raw reply could in principle be malformed too). */
function normalizeComment(raw, rawAnchor) {
  if (!isPlainObject(raw)) return null;

  const author = isPlainObject(raw.author) ? raw.author : {};
  const rawReplies = Array.isArray(raw.comments) ? raw.comments : [];
  const replies = rawReplies.map((reply) => normalizeComment(reply, undefined)).filter(Boolean);

  return {
    id: typeof raw.id === "number" ? raw.id : null,
    version: typeof raw.version === "number" ? raw.version : null,
    text: typeof raw.text === "string" ? raw.text : null,
    authorSlug: typeof author.slug === "string" ? author.slug : null,
    authorName:
      typeof author.displayName === "string"
        ? author.displayName
        : typeof author.name === "string"
          ? author.name
          : null,
    // Bitbucket's own epoch-millis number, kept as-is (no Date wrapping —
    // that's a display-layer concern), or null if missing.
    createdAt: typeof raw.createdDate === "number" ? raw.createdDate : null,
    severity: normalizeSeverity(raw.severity),
    state: normalizeState(raw.state),
    threadResolved: normalizeThreadResolved(raw.threadResolved),
    anchor: normalizeAnchor(rawAnchor),
    replies,
  };
}

function toPageArray(pages) {
  if (Array.isArray(pages)) return pages;
  // normalizeActivities(pages) is documented to also accept a single page
  // object directly (not wrapped in an array) — tolerate that here rather
  // than making every caller remember to wrap it.
  if (isPlainObject(pages)) return [pages];
  return [];
}

/**
 * `pages` is an array of paged `GET .../activities` responses
 * (`{values, isLastPage, ...}`), or a single such object, or anything else
 * (returns `[]` defensively). Returns `ReviewComment[]`: only `COMMENTED`
 * activities contribute, comments are deduped by id (across all pages,
 * and across activities — the same comment can appear more than once as it
 * gets edited), and the highest `version` wins.
 */
function normalizeActivities(pages) {
  const byId = new Map();

  for (const page of toPageArray(pages)) {
    if (!isPlainObject(page) || !Array.isArray(page.values)) continue;

    for (const activity of page.values) {
      if (!isPlainObject(activity) || activity.action !== "COMMENTED") continue;

      const comment = normalizeComment(activity.comment, activity.commentAnchor);
      if (!comment || comment.id === null) continue;

      const existing = byId.get(comment.id);
      const existingVersion = existing && existing.version !== null ? existing.version : -Infinity;
      const thisVersion = comment.version !== null ? comment.version : -Infinity;
      if (!existing || thisVersion > existingVersion) {
        byId.set(comment.id, comment);
      }
    }
  }

  return Array.from(byId.values());
}

/** Parses "8.19.1" (or any "MAJOR.MINOR..." prefix) into `{major, minor}`.
 * Anything unparseable (missing, not a string, no leading "N.N") maps to
 * `{major: 0, minor: 0}` — deliberately below every capability threshold,
 * so an unrecognized version is treated as the oldest possible one. */
function parseVersion(version) {
  if (typeof version !== "string") return { major: 0, minor: 0 };
  const match = version.match(/^(\d+)\.(\d+)/);
  if (!match) return { major: 0, minor: 0 };
  return { major: Number(match[1]), minor: Number(match[2]) };
}

/** `raw` is a `GET /rest/api/1.0/application-properties` response. Returns
 * `{version, major, minor}`, or `null` if `raw` isn't an object. */
function normalizeAppProperties(raw) {
  if (!isPlainObject(raw)) return null;
  const version = typeof raw.version === "string" ? raw.version : null;
  const { major, minor } = parseVersion(version);
  return { version, major, minor };
}

/**
 * `version` is either the `{major, minor}` normalizeAppProperties
 * produces, or a raw version string — accepted directly too so a caller
 * that only has the string doesn't need to round-trip it through
 * normalizeAppProperties first. Anything else (missing, malformed) is
 * treated as `{major: 0, minor: 0}`, which is below every threshold checked
 * below and so gets the conservative (everything off) capabilities — the
 * safe assumption when we don't actually know the server's version.
 */
function capabilitiesFor(version) {
  let major = 0;
  let minor = 0;
  if (typeof version === "string") {
    ({ major, minor } = parseVersion(version));
  } else if (isPlainObject(version) && typeof version.major === "number" && typeof version.minor === "number") {
    major = version.major;
    minor = version.minor;
  }

  return {
    // threadResolved (on a comment) appears in Bitbucket Server/DC 8.x+.
    threadResolvedField: major >= 8,
    // /rest/api/latest/.../commits/{sha}/builds needs 7.14+.
    commitBuildsEndpoint: major > 7 || (major === 7 && minor >= 14),
  };
}

// ---- Phase 6: ticket workspace / ticket to PR ----

/** The first `links.self[].href` of a raw PR (its page URL), or null. */
function selfHref(raw) {
  const links = isPlainObject(raw.links) ? raw.links : {};
  const self = Array.isArray(links.self) ? links.self : [];
  const first = self.find((l) => isPlainObject(l) && typeof l.href === "string");
  return first ? first.href : null;
}

/** normalizePullRequest plus the two fields a ticket view needs: the PR's
 * page `url` and `updatedAt` (epoch ms). Null for a non-object. */
function normalizePullRequestSummary(raw) {
  const base = normalizePullRequest(raw);
  if (!base) return null;
  return {
    ...base,
    url: selfHref(raw),
    updatedAt: typeof raw.updatedDate === "number" ? raw.updatedDate : null,
  };
}

/** A paged `GET .../pull-requests` response -> PR summaries (entries with
 * no numeric id are dropped). Anything else -> []. */
function normalizePullRequestList(raw) {
  if (!isPlainObject(raw) || !Array.isArray(raw.values)) return [];
  return raw.values.map(normalizePullRequestSummary).filter((pr) => pr && pr.id !== null);
}

const BUILD_STATES = ["FAILED", "INPROGRESS", "SUCCESSFUL"];

/** A paged build-status response (either endpoint) -> `{state, counts,
 * builds}`; `state` is the worst of the builds (FAILED, then INPROGRESS,
 * then SUCCESSFUL), or null when there are none. Unknown states count as
 * "unknown" and never make the summary look green. */
function normalizeBuildStatuses(raw) {
  const values = isPlainObject(raw) && Array.isArray(raw.values) ? raw.values : [];
  const builds = values.filter(isPlainObject).map((b) => ({
    state: BUILD_STATES.includes(b.state) ? b.state : "unknown",
    key: typeof b.key === "string" ? b.key : null,
    name: typeof b.name === "string" ? b.name : null,
    url: typeof b.url === "string" ? b.url : null,
    at: typeof b.dateAdded === "number" ? b.dateAdded : null,
  }));
  const counts = { SUCCESSFUL: 0, FAILED: 0, INPROGRESS: 0, unknown: 0 };
  for (const b of builds) counts[b.state] += 1;
  let state = null;
  if (counts.FAILED > 0) state = "FAILED";
  else if (counts.INPROGRESS > 0 || counts.unknown > 0) state = "INPROGRESS";
  else if (counts.SUCCESSFUL > 0) state = "SUCCESSFUL";
  return { state, counts, builds: builds.slice(0, 10) };
}

/** A default-branch response (`{id: "refs/heads/master", displayId}`) ->
 * the plain branch name, or null. */
function normalizeDefaultBranch(raw) {
  if (!isPlainObject(raw)) return null;
  if (typeof raw.displayId === "string" && raw.displayId) return raw.displayId;
  if (typeof raw.id === "string" && raw.id.startsWith("refs/heads/")) return raw.id.slice("refs/heads/".length) || null;
  return null;
}

/** `GET .../repos/{slug}` -> `{id, slug, projectKey}`, or null without a numeric id. */
function normalizeRepository(raw) {
  if (!isPlainObject(raw) || typeof raw.id !== "number") return null;
  const project = isPlainObject(raw.project) ? raw.project : {};
  return {
    id: raw.id,
    slug: typeof raw.slug === "string" ? raw.slug : null,
    projectKey: typeof project.key === "string" ? project.key : null,
  };
}

/** The default-reviewers plugin's user array -> unique usernames (`name`,
 * else `slug`) of active users. Anything else -> []. */
function normalizeDefaultReviewers(raw) {
  const users = Array.isArray(raw) ? raw : [];
  const names = [];
  for (const u of users) {
    if (!isPlainObject(u) || u.active === false) continue;
    const name = typeof u.name === "string" && u.name ? u.name : typeof u.slug === "string" ? u.slug : null;
    if (name && !names.includes(name)) names.push(name);
  }
  return names;
}

// ---- Phase 7: the watchers and the digest ----

const REVIEWER_STATUSES = ["APPROVED", "NEEDS_WORK", "UNAPPROVED"];

/** A dashboard PR: the summary plus its repo (from toRef), reviewers
 * (`{name, status}`, unknown statuses as "unknown") and approval count.
 * Null without an id, project or repo. */
function normalizeDashboardPullRequest(raw) {
  const base = normalizePullRequestSummary(raw);
  if (!base || base.id === null || !base.toRepo.projectKey || !base.toRepo.slug) return null;
  const reviewers = (Array.isArray(raw.reviewers) ? raw.reviewers : [])
    .filter((r) => isPlainObject(r) && isPlainObject(r.user))
    .map((r) => ({
      name: typeof r.user.name === "string" ? r.user.name : typeof r.user.slug === "string" ? r.user.slug : null,
      status: REVIEWER_STATUSES.includes(r.status) ? r.status : r.approved === true ? "APPROVED" : "unknown",
    }))
    .filter((r) => r.name);
  return {
    ...base,
    project: base.toRepo.projectKey,
    repo: base.toRepo.slug,
    reviewers,
    approvals: reviewers.filter((r) => r.status === "APPROVED").length,
  };
}

function normalizeDashboardPullRequests(raw) {
  if (!isPlainObject(raw) || !Array.isArray(raw.values)) return [];
  return raw.values.map(normalizeDashboardPullRequest).filter(Boolean);
}

/** `GET .../merge` -> `{conflicted, canMerge, outcome}` (null when absent). */
function normalizeMergeStatus(raw) {
  if (!isPlainObject(raw)) return { conflicted: null, canMerge: null, outcome: null };
  return {
    conflicted: typeof raw.conflicted === "boolean" ? raw.conflicted : raw.outcome === "CONFLICTED" ? true : null,
    canMerge: typeof raw.canMerge === "boolean" ? raw.canMerge : null,
    outcome: typeof raw.outcome === "string" ? raw.outcome : null,
  };
}

module.exports = {
  normalizePullRequest,
  normalizeDashboardPullRequest,
  normalizeDashboardPullRequests,
  normalizeMergeStatus,
  normalizePullRequestSummary,
  normalizePullRequestList,
  normalizeBuildStatuses,
  normalizeDefaultBranch,
  normalizeRepository,
  normalizeDefaultReviewers,
  normalizeActivities,
  normalizeAppProperties,
  capabilitiesFor,
  EXPECTED_KEYS,
  OPTIONAL_KEYS,
};
