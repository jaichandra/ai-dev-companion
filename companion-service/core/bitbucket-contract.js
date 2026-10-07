// Pure logic behind `doctor --bitbucket-contract [PR url]` (Task 5). Kept
// separate from core/bitbucket.ts (which does the actual network calls) so
// this can be unit-tested with `node --test` and no build step — see the
// Global Constraint that pure logic worth testing lives in plain JS.
//
// Four independent pieces:
//   - parsePrUrl: turns a pasted Bitbucket PR URL into {project, repo, id}.
//   - missingKeys / missingKeysAcross: diff a live raw response's keys
//     against core/bitbucket-normalize.js's EXPECTED_KEYS, without
//     duplicating that list.
//   - followPages: the same "follow nextPageStart until isLastPage, capped"
//     shape core/bitbucket.ts's listActivities uses, but with the actual
//     fetch injected — so both this file's tests and bitbucket.ts's real
//     fetcher share one implementation.
//   - compareVersion: whether the live Bitbucket version differs from what
//     was last recorded to stateDir()/bitbucket-version.json.

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Parses `https://host/projects/P/repos/R/pull-requests/N[/...]` into
 * `{project, repo, id}` (id as a number). Anything after the PR number
 * (diff, overview, a #comment-N fragment, ...) is ignored. Returns null for
 * anything that doesn't parse as a URL, or whose path doesn't have this
 * shape — never throws, since this is user-pasted input.
 */
function parsePrUrl(url) {
  if (typeof url !== "string") return null;
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  const match = parsed.pathname.match(/\/projects\/([^/]+)\/repos\/([^/]+)\/pull-requests\/(\d+)(?:\/|$)/);
  if (!match) return null;
  return { project: match[1], repo: match[2], id: Number(match[3]) };
}

/** Whether `dottedPath` (e.g. "fromRef.displayId") resolves to a defined
 * value inside `obj` — an explicit `null` counts as present (the field
 * exists, Bitbucket just has nothing to say), only a genuinely absent key
 * (or a missing intermediate object) counts as missing. Never throws on a
 * non-object `obj` or a non-object intermediate value. */
function hasDottedKey(obj, dottedPath) {
  const segments = dottedPath.split(".");
  let cur = obj;
  for (const segment of segments) {
    if (!isPlainObject(cur) || !(segment in cur)) return false;
    cur = cur[segment];
  }
  return true;
}

/**
 * Which of `expectedKeys` (dotted paths, straight from
 * core/bitbucket-normalize.js's EXPECTED_KEYS) are missing from a single
 * raw response object. A non-object `raw` (the endpoint returned nothing
 * usable) reports every key missing, since none of them can be checked.
 */
function missingKeys(raw, expectedKeys) {
  if (!isPlainObject(raw)) return [...expectedKeys];
  return expectedKeys.filter((key) => !hasDottedKey(raw, key));
}

/**
 * Same idea as missingKeys, but across several raw samples of the same
 * shape — needed for `activity`/`comment`/`commentAnchor`, which come from
 * a *list* of PR activities rather than one object. A key only counts as
 * missing if it's absent from every sample: Bitbucket only sets
 * `commentAnchor` on comments that anchor to a diff line, so checking a
 * single activity would misreport a legitimately-absent field as a broken
 * contract. Non-object samples (e.g. an activity with no `commentAnchor`
 * at all) are tolerated, not pre-filtering required.
 *
 * Returns `[]` for an empty `samples` — "nothing to check" (e.g. a PR with
 * no anchored comments at all), never "everything is missing".
 */
function missingKeysAcross(samples, expectedKeys) {
  if (samples.length === 0) return [];
  return expectedKeys.filter((key) => !samples.some((sample) => hasDottedKey(sample, key)));
}

/**
 * Follows a paged Bitbucket endpoint (`{values, isLastPage, nextPageStart}`)
 * by calling `fetchPage(start)` — starting at 0, then at each page's own
 * `nextPageStart` — until a page reports `isLastPage`, or `maxPages` pages
 * have been fetched, whichever comes first. `fetchPage` is the only I/O;
 * this function is otherwise pure, which is what makes it testable with a
 * fake here and reusable, unchanged, by core/bitbucket.ts's real fetcher.
 *
 * Returns `{pages, truncated}` — `truncated` is true when the cap was hit
 * before `isLastPage`, so a caller can log a WARN instead of failing (see
 * this task's ruling: hitting the cap is not an error). A page it can't
 * follow (not an object, or no numeric `nextPageStart` on a non-last
 * page) ends the walk the same way — WARN via `warn`, return what was
 * collected — with `malformed: true` added.
 */
async function followPages(fetchPage, { maxPages = 20, warn = console.warn } = {}) {
  const pages = [];
  let start = 0;
  for (let i = 0; i < maxPages; i += 1) {
    const page = await fetchPage(start);
    // A page that isn't an object, or one that says there's more but not
    // where it starts, can't be followed. Stop with what's collected so
    // far and a WARN instead of throwing on `page.isLastPage` or asking
    // for `start=undefined` (which Bitbucket reads as 0 — the same page
    // again, until the cap). `malformed` lets a caller tell this apart
    // from hitting the cap.
    if (!isPlainObject(page)) {
      warn(`[bitbucket] page ${i + 1} (start=${start}) was not an object — stopping with ${pages.length} page(s).`);
      return { pages, truncated: false, malformed: true };
    }
    pages.push(page);
    if (page.isLastPage) return { pages, truncated: false };
    if (typeof page.nextPageStart !== "number" || !Number.isFinite(page.nextPageStart)) {
      warn(
        `[bitbucket] page ${i + 1} (start=${start}) isn't the last page but has no numeric nextPageStart — ` +
          `stopping with ${pages.length} page(s).`,
      );
      return { pages, truncated: false, malformed: true };
    }
    start = page.nextPageStart;
  }
  return { pages, truncated: true };
}

/**
 * Compares the previously recorded `{version, seenAt}` (from
 * stateDir()/bitbucket-version.json, or null/undefined if this is the
 * first run) against the live `version` string. Returns
 * `{changed, message}` — `message` is a ready-to-print line when the
 * version moved, or null when there's nothing to say (first run, or no
 * change).
 */
function compareVersion(recorded, live) {
  if (!recorded || !recorded.version || recorded.version === live) {
    return { changed: false, message: null };
  }
  return {
    changed: true,
    message: `Bitbucket changed from ${recorded.version} to ${live} — re-check the contract.`,
  };
}

module.exports = {
  parsePrUrl,
  missingKeys,
  missingKeysAcross,
  followPages,
  compareVersion,
};
