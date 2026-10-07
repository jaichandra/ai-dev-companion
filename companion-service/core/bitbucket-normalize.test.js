const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const path = require("path");
const {
  normalizePullRequest,
  normalizeActivities,
  normalizeAppProperties,
  capabilitiesFor,
  EXPECTED_KEYS,
  OPTIONAL_KEYS,
} = require("./bitbucket-normalize.js");

const FIXTURES_ROOT = path.join(__dirname, "fixtures", "bitbucket");

/** Every fixture directory under fixtures/bitbucket/ (docs-8.x today, a
 * future real-9.x/ automatically once someone adds it — see its README). */
function fixtureDirs() {
  return fs
    .readdirSync(FIXTURES_ROOT, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();
}

function readJson(dir, name) {
  return JSON.parse(fs.readFileSync(path.join(FIXTURES_ROOT, dir, name), "utf8"));
}

/** All activities-page*.json files in a fixture dir, in filename order —
 * a dir may have more or fewer than two pages. */
function readActivityPages(dir) {
  return fs
    .readdirSync(path.join(FIXTURES_ROOT, dir))
    .filter((name) => /^activities-page\d+\.json$/.test(name))
    .sort()
    .map((name) => readJson(dir, name));
}

function get(obj, dottedPath) {
  return dottedPath.split(".").reduce((acc, key) => {
    return acc !== null && typeof acc === "object" ? acc[key] : undefined;
  }, obj);
}

// ---------------------------------------------------------------------------
// Generic invariants: run over every fixture directory found, so a future
// real-9.x/ (see fixtures/bitbucket/README.md) is exercised automatically.
// ---------------------------------------------------------------------------

for (const dir of fixtureDirs()) {
  test(`${dir}: normalizePullRequest returns the stable shape`, () => {
    const raw = readJson(dir, "pull-request.json");
    const pr = normalizePullRequest(raw);
    assert.equal(typeof pr.id, "number");
    assert.equal(typeof pr.version, "number");
    assert.equal(typeof pr.title, "string");
    assert.equal(typeof pr.state, "string");
    assert.equal(typeof pr.fromBranch, "string");
    assert.equal(typeof pr.fromSha, "string");
    assert.equal(typeof pr.toBranch, "string");
    assert.equal(typeof pr.toSha, "string");
    assert.equal(typeof pr.authorSlug, "string");
  });

  test(`${dir}: normalizeActivities returns comments with no duplicate ids and the highest version wins`, () => {
    const pages = readActivityPages(dir);
    const comments = normalizeActivities(pages);
    assert.ok(Array.isArray(comments));
    assert.ok(comments.length > 0);

    const ids = comments.map((c) => c.id);
    assert.equal(new Set(ids).size, ids.length, "no duplicate top-level comment ids");

    for (const c of comments) {
      assert.equal(typeof c.id, "number");
      assert.ok(c.authorSlug === null || typeof c.authorSlug === "string");
      assert.ok(c.authorName === null || typeof c.authorName === "string");
      assert.ok(c.createdAt === null || typeof c.createdAt === "number");
      assert.ok(["NORMAL", "BLOCKER", "unknown"].includes(c.severity));
      assert.ok(["OPEN", "RESOLVED", "PENDING", "unknown"].includes(c.state));
      assert.ok(c.threadResolved === null || typeof c.threadResolved === "boolean");
      assert.ok(Array.isArray(c.replies));
      if (c.anchor !== null) {
        assert.ok(["ADDED", "REMOVED", "CONTEXT", "unknown"].includes(c.anchor.lineType));
        assert.ok(["FROM", "TO", "unknown"].includes(c.anchor.fileType));
        assert.ok(["EFFECTIVE", "COMMIT", "RANGE", "unknown"].includes(c.anchor.diffType));
        assert.equal(typeof c.anchor.orphaned, "boolean");
      }
    }
  });

  test(`${dir}: normalizeActivities only includes COMMENTED activities`, () => {
    const pages = readActivityPages(dir);
    const rawCommentedIds = new Set();
    for (const page of pages) {
      for (const activity of page.values) {
        if (activity.action === "COMMENTED") rawCommentedIds.add(activity.comment.id);
      }
    }
    const comments = normalizeActivities(pages);
    for (const c of comments) {
      assert.ok(rawCommentedIds.has(c.id), `comment ${c.id} came from a non-COMMENTED activity`);
    }
  });

  test(`${dir}: normalizeAppProperties + capabilitiesFor round-trip without throwing`, () => {
    const raw = readJson(dir, "application-properties.json");
    const props = normalizeAppProperties(raw);
    assert.equal(typeof props.major, "number");
    assert.equal(typeof props.minor, "number");
    const caps = capabilitiesFor(props);
    assert.equal(typeof caps.threadResolvedField, "boolean");
    assert.equal(typeof caps.commitBuildsEndpoint, "boolean");
  });
}

// ---------------------------------------------------------------------------
// docs-8.x: exact scenario coverage named in the task brief.
// ---------------------------------------------------------------------------

test("docs-8.x: normalizePullRequest maps every field verbatim", () => {
  const raw = readJson("docs-8.x", "pull-request.json");
  const pr = normalizePullRequest(raw);
  assert.deepEqual(pr, {
    id: 42,
    version: 3,
    title: "Fix widget rendering on Safari",
    state: "OPEN",
    fromBranch: "fix/widget-flicker",
    fromSha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    fromRepo: { projectKey: "ACME", slug: "sample-app" },
    toBranch: "master",
    toSha: "0f1e2d3c4b5a69788796a5b4c3d2e1f0a9b8c7d6",
    toRepo: { projectKey: "ACME", slug: "sample-app" },
    authorSlug: "jsmith",
    openTaskCount: 1,
    commentCount: 8,
  });
});

test("docs-8.x: normalizeActivities covers open comment, resolved thread, BLOCKER, reply, orphaned/REMOVED/COMMIT anchors, dedup and unknown enums", () => {
  const pages = readActivityPages("docs-8.x");
  const comments = normalizeActivities(pages);
  const byId = new Map(comments.map((c) => [c.id, c]));

  // Filtered: two non-COMMENTED activities (OPENED, APPROVED) contribute no comment.
  assert.equal(comments.length, 9, "9 distinct top-level comments (ids 1,2,3,4,5,6,7,9,10)");

  // Open comment.
  const open = byId.get(1);
  assert.equal(open.state, "OPEN");
  assert.equal(open.severity, "NORMAL");
  assert.equal(open.threadResolved, false);
  // Dedup: id 1 appears on both pages (v1, v2) — the higher version wins.
  assert.equal(open.version, 2);
  assert.equal(open.text, "Please rename this variable for clarity — done, thanks!");
  // createdAt keeps Bitbucket's raw epoch-millis number verbatim (no Date
  // wrapping), and authorSlug/authorName come off the comment's author.
  assert.equal(open.createdAt, 1706000100000);
  assert.equal(open.authorSlug, "asmith");
  assert.equal(open.authorName, "Alice Smith");

  // Resolved thread.
  const resolved = byId.get(2);
  assert.equal(resolved.state, "RESOLVED");
  assert.equal(resolved.threadResolved, true);

  // BLOCKER task.
  const blocker = byId.get(3);
  assert.equal(blocker.severity, "BLOCKER");

  // Reply, nested recursively.
  const withReply = byId.get(4);
  assert.equal(withReply.replies.length, 1);
  assert.equal(withReply.replies[0].id, 8);
  assert.equal(withReply.replies[0].text, "Good idea, added in the latest push.");
  assert.equal(withReply.replies[0].anchor, null, "a reply has no anchor of its own");
  assert.equal(withReply.replies[0].createdAt, 1706000450000);
  assert.equal(withReply.replies[0].authorSlug, "asmith");
  assert.equal(withReply.replies[0].authorName, "Alice Smith");

  // Orphaned anchor.
  const orphaned = byId.get(5);
  assert.equal(orphaned.anchor.orphaned, true);

  // REMOVED-line anchor.
  const removedLine = byId.get(6);
  assert.equal(removedLine.anchor.lineType, "REMOVED");
  assert.equal(removedLine.anchor.fileType, "FROM");

  // COMMIT-diff anchor.
  const commitDiff = byId.get(7);
  assert.equal(commitDiff.anchor.diffType, "COMMIT");

  // Unrecognized severity/state map to "unknown", never throw.
  const weird = byId.get(9);
  assert.equal(weird.severity, "unknown");
  assert.equal(weird.state, "unknown");

  // No commentAnchor on the activity at all -> anchor is null (not just an
  // orphaned/empty object).
  const noAnchor = byId.get(10);
  assert.equal(noAnchor.anchor, null);
});

test("docs-8.x: capabilitiesFor(8.19) turns on both capabilities", () => {
  const props = normalizeAppProperties(readJson("docs-8.x", "application-properties.json"));
  assert.deepEqual(capabilitiesFor(props), {
    threadResolvedField: true,
    commitBuildsEndpoint: true,
  });
});

test("docs-8.x: normalizeAppProperties parses major/minor from the version string", () => {
  const props = normalizeAppProperties(readJson("docs-8.x", "application-properties.json"));
  assert.equal(props.version, "8.19.1");
  assert.equal(props.major, 8);
  assert.equal(props.minor, 19);
});

// ---------------------------------------------------------------------------
// capabilitiesFor: the version-threshold table, independent of fixtures.
// ---------------------------------------------------------------------------

test("capabilitiesFor: threadResolvedField is on for 8.x+ and off below it", () => {
  assert.equal(capabilitiesFor({ major: 8, minor: 0 }).threadResolvedField, true);
  assert.equal(capabilitiesFor({ major: 9, minor: 0 }).threadResolvedField, true);
  assert.equal(capabilitiesFor({ major: 7, minor: 20 }).threadResolvedField, false);
});

test("capabilitiesFor: commitBuildsEndpoint needs 7.14+", () => {
  assert.equal(capabilitiesFor({ major: 7, minor: 14 }).commitBuildsEndpoint, true);
  assert.equal(capabilitiesFor({ major: 7, minor: 13 }).commitBuildsEndpoint, false);
  assert.equal(capabilitiesFor({ major: 8, minor: 0 }).commitBuildsEndpoint, true);
  assert.equal(capabilitiesFor({ major: 6, minor: 99 }).commitBuildsEndpoint, false);
});

test("capabilitiesFor: an unparseable/below-range version gets the conservative values", () => {
  assert.deepEqual(capabilitiesFor({ major: 0, minor: 0 }), {
    threadResolvedField: false,
    commitBuildsEndpoint: false,
  });
});

test("normalizeAppProperties: an unparseable version string parses to major 0, minor 0", () => {
  const props = normalizeAppProperties({ version: "not-a-version" });
  assert.equal(props.major, 0);
  assert.equal(props.minor, 0);
});

test("normalizeAppProperties: parses a plain major.minor.patch version", () => {
  const props = normalizeAppProperties({ version: "8.19.1" });
  assert.equal(props.major, 8);
  assert.equal(props.minor, 19);
});

// ---------------------------------------------------------------------------
// Defensive reading: never throw, sensible defaults for missing/malformed input.
// ---------------------------------------------------------------------------

test("normalizePullRequest returns null for non-object input", () => {
  assert.equal(normalizePullRequest(null), null);
  assert.equal(normalizePullRequest(undefined), null);
  assert.equal(normalizePullRequest("nope"), null);
  assert.equal(normalizePullRequest(42), null);
});

test("normalizePullRequest tolerates a raw object missing every field", () => {
  const pr = normalizePullRequest({});
  assert.equal(pr.id, null);
  assert.equal(pr.version, null);
  assert.equal(pr.title, null);
  assert.equal(pr.state, null);
  assert.equal(pr.fromBranch, null);
  assert.equal(pr.fromSha, null);
  assert.equal(pr.toBranch, null);
  assert.equal(pr.toSha, null);
  assert.deepEqual(pr.fromRepo, { projectKey: null, slug: null });
  assert.deepEqual(pr.toRepo, { projectKey: null, slug: null });
  assert.equal(pr.authorSlug, null);
  assert.equal("openTaskCount" in pr, false);
  assert.equal("commentCount" in pr, false);
});

test("normalizeActivities returns [] for non-array, non-object input", () => {
  assert.deepEqual(normalizeActivities(null), []);
  assert.deepEqual(normalizeActivities(undefined), []);
  assert.deepEqual(normalizeActivities("nope"), []);
  assert.deepEqual(normalizeActivities(42), []);
});

test("normalizeActivities tolerates a single page object instead of an array of pages", () => {
  const page = readJson("docs-8.x", "activities-page1.json");
  const fromArray = normalizeActivities([page]);
  const fromSingleObject = normalizeActivities(page);
  assert.deepEqual(fromSingleObject, fromArray);
});

test("normalizeActivities tolerates a page missing/malformed values", () => {
  assert.deepEqual(normalizeActivities([{ isLastPage: true }]), []);
  assert.deepEqual(normalizeActivities([{ values: "nope" }]), []);
  assert.deepEqual(normalizeActivities([null, undefined, { values: [] }]), []);
});

test("normalizeActivities skips a COMMENTED activity with a missing/non-object comment", () => {
  assert.deepEqual(normalizeActivities([{ values: [{ action: "COMMENTED" }] }]), []);
  assert.deepEqual(
    normalizeActivities([{ values: [{ action: "COMMENTED", comment: "nope" }] }]),
    [],
  );
});

test("normalizeActivities: createdAt/authorSlug/authorName default to null when author/createdDate are missing", () => {
  const [comment] = normalizeActivities([
    { values: [{ action: "COMMENTED", comment: { id: 1, version: 1, text: "no author or date" } }] },
  ]);
  assert.equal(comment.createdAt, null);
  assert.equal(comment.authorSlug, null);
  assert.equal(comment.authorName, null);
});

test("normalizeActivities: authorName falls back to author.name when displayName is absent", () => {
  const [comment] = normalizeActivities([
    {
      values: [
        {
          action: "COMMENTED",
          comment: {
            id: 1,
            version: 1,
            text: "old-style author",
            author: { name: "bwhite", slug: "bwhite" },
          },
        },
      ],
    },
  ]);
  assert.equal(comment.authorSlug, "bwhite");
  assert.equal(comment.authorName, "bwhite");
});

test("normalizeActivities: anchor.orphaned defaults to false when the raw anchor omits it", () => {
  const [comment] = normalizeActivities([
    {
      values: [
        {
          action: "COMMENTED",
          comment: { id: 1, version: 1, text: "x" },
          commentAnchor: { path: "src/x.js", line: 1, lineType: "CONTEXT", fileType: "TO", diffType: "EFFECTIVE" },
        },
      ],
    },
  ]);
  assert.equal(comment.anchor.orphaned, false);
});

test("normalizeActivities: a non-array comment.comments normalizes to replies: []", () => {
  const [comment] = normalizeActivities([
    { values: [{ action: "COMMENTED", comment: { id: 1, version: 1, text: "x", comments: "not-an-array" } }] },
  ]);
  assert.deepEqual(comment.replies, []);
});

test("normalizeActivities: a non-object commentAnchor normalizes to anchor: null", () => {
  const [comment] = normalizeActivities([
    { values: [{ action: "COMMENTED", comment: { id: 1, version: 1, text: "x" }, commentAnchor: "nope" }] },
  ]);
  assert.equal(comment.anchor, null);
});

test("normalizeAppProperties returns null for non-object input", () => {
  assert.equal(normalizeAppProperties(null), null);
  assert.equal(normalizeAppProperties(undefined), null);
  assert.equal(normalizeAppProperties("nope"), null);
});

test("capabilitiesFor tolerates a missing/malformed version defensively (conservative)", () => {
  assert.deepEqual(capabilitiesFor(null), { threadResolvedField: false, commitBuildsEndpoint: false });
  assert.deepEqual(capabilitiesFor(undefined), { threadResolvedField: false, commitBuildsEndpoint: false });
  assert.deepEqual(capabilitiesFor({}), { threadResolvedField: false, commitBuildsEndpoint: false });
});

// ---------------------------------------------------------------------------
// EXPECTED_KEYS: the single source of truth Task 5's live contract check
// compares real responses against. This test makes sure it's complete —
// every raw key a fixture supplies for a field the normalizers actually
// read is listed under the right raw-object-type key.
// ---------------------------------------------------------------------------

test("EXPECTED_KEYS covers every raw path normalizePullRequest reads, for every fixture", () => {
  const prPaths = [
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
  ];
  for (const p of prPaths) {
    assert.ok(EXPECTED_KEYS.pullRequest.includes(p), `EXPECTED_KEYS.pullRequest is missing "${p}"`);
  }
  for (const dir of fixtureDirs()) {
    const raw = readJson(dir, "pull-request.json");
    for (const p of prPaths) {
      assert.notEqual(get(raw, p), undefined, `${dir}/pull-request.json has no value at "${p}"`);
    }
  }
});

// OPTIONAL_KEYS.pullRequest: normalizePullRequest reads properties.
// openTaskCount/commentCount, but a real Bitbucket instance (confirmed on
// 9.4.16) may not return `properties` on this endpoint at all — so, unlike
// EXPECTED_KEYS above, these paths are NOT required to be present in every
// fixture, only listed under OPTIONAL_KEYS so doctor's contract check can
// tell "known optional gap" apart from "the contract broke". docs-8.x is
// modeled with `properties` present, so it's used here to confirm the paths
// still resolve where the fixture does supply them.
test("OPTIONAL_KEYS.pullRequest lists properties.openTaskCount/commentCount, present in docs-8.x", () => {
  const optionalPrPaths = ["properties.openTaskCount", "properties.commentCount"];
  for (const p of optionalPrPaths) {
    assert.ok(OPTIONAL_KEYS.pullRequest.includes(p), `OPTIONAL_KEYS.pullRequest is missing "${p}"`);
    // These must NOT also be required — a fixture (or a real instance)
    // that omits `properties` shouldn't fail the EXPECTED_KEYS check above.
    assert.ok(!EXPECTED_KEYS.pullRequest.includes(p), `EXPECTED_KEYS.pullRequest should not include "${p}"`);
  }
  const raw = readJson("docs-8.x", "pull-request.json");
  for (const p of optionalPrPaths) {
    assert.notEqual(get(raw, p), undefined, `docs-8.x/pull-request.json has no value at "${p}"`);
  }
});

test("EXPECTED_KEYS covers every raw path normalizeActivities/comment/commentAnchor reads", () => {
  const activityPaths = ["action", "comment", "commentAnchor"];
  const commentPaths = [
    "id",
    "version",
    "text",
    "author.slug",
    "author.displayName",
    "createdDate",
    "state",
    "severity",
    "threadResolved",
    "comments",
  ];
  const anchorPaths = ["path", "line", "lineType", "fileType", "diffType", "orphaned"];

  for (const p of activityPaths) assert.ok(EXPECTED_KEYS.activity.includes(p));
  for (const p of commentPaths) assert.ok(EXPECTED_KEYS.comment.includes(p), `EXPECTED_KEYS.comment is missing "${p}"`);
  for (const p of anchorPaths) assert.ok(EXPECTED_KEYS.commentAnchor.includes(p), `EXPECTED_KEYS.commentAnchor is missing "${p}"`);

  // Every commentPath must be exercised by at least one comment somewhere
  // in the fixtures (not necessarily every comment — e.g. a reply has no
  // commentAnchor at the activity level, and that's fine), and every
  // anchorPath by at least one commentAnchor.
  const seenCommentPaths = new Set();
  const seenAnchorPaths = new Set();
  for (const dir of fixtureDirs()) {
    for (const page of readActivityPages(dir)) {
      for (const activity of page.values) {
        if (activity.action !== "COMMENTED") continue;
        for (const p of commentPaths) {
          if (get(activity.comment, p) !== undefined) seenCommentPaths.add(p);
        }
        if (activity.commentAnchor) {
          for (const p of anchorPaths) {
            if (get(activity.commentAnchor, p) !== undefined) seenAnchorPaths.add(p);
          }
        }
      }
    }
  }
  for (const p of commentPaths) {
    assert.ok(seenCommentPaths.has(p), `no fixture comment ever supplies "${p}"`);
  }
  for (const p of anchorPaths) {
    assert.ok(seenAnchorPaths.has(p), `no fixture commentAnchor ever supplies "${p}"`);
  }
});

test("EXPECTED_KEYS covers every raw path normalizeAppProperties reads, for every fixture", () => {
  assert.ok(EXPECTED_KEYS.applicationProperties.includes("version"));
  for (const dir of fixtureDirs()) {
    const raw = readJson(dir, "application-properties.json");
    assert.notEqual(raw.version, undefined);
  }
});

test("normalizePullRequest reads a fork PR's source repo apart from its target, and tolerates junk repository shapes", () => {
  const fork = normalizePullRequest({
    fromRef: { repository: { slug: "sample-app", project: { key: "~JSMITH" } } },
    toRef: { repository: { slug: "sample-app", project: { key: "ACME" } } },
  });
  assert.deepEqual(fork.fromRepo, { projectKey: "~JSMITH", slug: "sample-app" });
  assert.deepEqual(fork.toRepo, { projectKey: "ACME", slug: "sample-app" });
  const junk = normalizePullRequest({
    fromRef: { repository: "nope" },
    toRef: { repository: { slug: 7, project: [] } },
  });
  assert.deepEqual(junk.fromRepo, { projectKey: null, slug: null });
  assert.deepEqual(junk.toRepo, { projectKey: null, slug: null });
});
