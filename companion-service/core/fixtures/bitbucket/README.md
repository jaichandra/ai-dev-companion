# Bitbucket fixtures

Each subdirectory here is a self-contained set of raw Bitbucket Server/DC
REST JSON, exercised by `core/bitbucket-normalize.test.js`. The test
discovers directories with `fs.readdirSync` and runs every normalizer over
every directory it finds — add a new directory and it is tested
automatically, no wiring required.

## `docs-8.x/`

Modeled by hand on **Atlassian's documented response shapes** for Bitbucket
Server/DC 8.x (pull-request, PR activities, application-properties) — see
`.superpowers/sdd/2026-09-26-phase-2-review-comments/global-constraints.md`'s
"Bitbucket Server/DC REST facts" section. **It is not a captured response
from a real instance.** It's a stand-in until a real Bitbucket instance is
available, at which point Task 5's live contract check compares real
responses against `bitbucket-normalize.js`'s exported `EXPECTED_KEYS` before
merge.

Files:
- `pull-request.json` — `GET .../pull-requests/{id}`.
- `activities-page1.json`, `activities-page2.json` — two pages of
  `GET .../pull-requests/{id}/activities`, together covering: an open
  comment, a resolved thread (`threadResolved: true`), a `BLOCKER` task, a
  reply (nested under a top-level comment's `comments[]`), an orphaned
  anchor, a `REMOVED`-line anchor, a `COMMIT`-diff anchor, a comment with no
  anchor at all, an unrecognized `severity`/`state` pair (exercises the
  `"unknown"` fallback), a non-`COMMENTED` activity (exercises the activity
  filter), and a second, higher-`version` copy of comment `id: 1` on page 2
  (exercises the dedup-by-id/highest-version rule).
- `application-properties.json` — `GET /rest/api/1.0/application-properties`,
  version `8.19.1` (major 8 — turns on `threadResolvedField` and, since
  8 > 7, `commitBuildsEndpoint`).

## A real-instance finding worth noting here

Confirmed against a real Bitbucket Server/DC 9.4.16 instance:
`GET /rest/api/1.0/projects/{p}/repos/{r}/pull-requests/{id}` (the
single-PR endpoint `pull-request.json` here models) returns **no**
`properties` key at all — its top-level keys are id, version, title,
description, state, open, closed, draft, createdDate, updatedDate,
fromRef, toRef, locked, author, reviewers, participants, links. Only the
*list* endpoint (`GET .../pull-requests?state=ALL&withProperties=true`)
returns `properties: {mergeResult, resolvedTaskCount, commentCount,
openTaskCount}`, per PR, and there's no single-PR equivalent. `docs-8.x/`
below still models `properties` as present on the single-PR response
(per Atlassian's documented shape), so `bitbucket-normalize.js`'s
`OPTIONAL_KEYS.pullRequest` — not `EXPECTED_KEYS` — is where
`properties.openTaskCount`/`commentCount` are listed, and
`doctor --bitbucket-contract` reports them missing as an informational OK,
not a WARN, when run against a 9.x instance.

## Adding a captured directory for a real instance

Once a real Bitbucket Server/DC instance is available, add a sibling
directory named `real-<version>/` (e.g. `real-9.2/`), with the *same* four
file names (`pull-request.json`, `activities-page1.json`,
`activities-page2.json`, `application-properties.json`), populated with an
actual (secrets-scrubbed) capture from that instance — a real PR's
activities may need more than two pages' worth of files if you want full
coverage; `bitbucket-normalize.test.js`'s generic assertions only assume at
least one `activities-page*.json` file. No code changes are needed: the
test suite picks the new directory up on the next run and exercises every
normalizer against it.
