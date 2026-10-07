const test = require("node:test");
const assert = require("node:assert/strict");
const {
  parsePrUrl,
  missingKeys,
  missingKeysAcross,
  followPages,
  compareVersion,
} = require("./bitbucket-contract.js");

// ---- parsePrUrl ----

test("parsePrUrl parses project/repo/id from a plain PR URL", () => {
  assert.deepEqual(
    parsePrUrl("https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/123"),
    { project: "ACME", repo: "sample-app", id: 123 },
  );
});

test("parsePrUrl ignores anything after the PR number", () => {
  assert.deepEqual(
    parsePrUrl("https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/123/diff#chg-foo.js"),
    { project: "ACME", repo: "sample-app", id: 123 },
  );
  assert.deepEqual(
    parsePrUrl("https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/123/overview"),
    { project: "ACME", repo: "sample-app", id: 123 },
  );
});

test("parsePrUrl accepts project/repo keys with dots, dashes and underscores", () => {
  assert.deepEqual(
    parsePrUrl("https://bitbucket.example.com/projects/ACME-1/repos/web.ui_2/pull-requests/7"),
    { project: "ACME-1", repo: "web.ui_2", id: 7 },
  );
});

test("parsePrUrl returns null for a URL missing the pull-requests segment", () => {
  assert.equal(parsePrUrl("https://bitbucket.example.com/projects/ACME/repos/sample-app"), null);
});

test("parsePrUrl returns null for a non-Bitbucket-shaped path", () => {
  assert.equal(parsePrUrl("https://bitbucket.example.com/dashboard"), null);
});

test("parsePrUrl returns null for unparseable input", () => {
  assert.equal(parsePrUrl("not a url"), null);
  assert.equal(parsePrUrl(undefined), null);
  assert.equal(parsePrUrl(null), null);
  assert.equal(parsePrUrl(42), null);
});

// ---- missingKeys ----

test("missingKeys reports nothing when every dotted key resolves", () => {
  const raw = { id: 1, fromRef: { displayId: "feature/x" } };
  assert.deepEqual(missingKeys(raw, ["id", "fromRef.displayId"]), []);
});

test("missingKeys reports top-level and nested keys that are absent", () => {
  const raw = { id: 1, fromRef: {} };
  assert.deepEqual(missingKeys(raw, ["id", "title", "fromRef.displayId"]), ["title", "fromRef.displayId"]);
});

test("missingKeys treats an explicit null value as present, not missing", () => {
  const raw = { state: null };
  assert.deepEqual(missingKeys(raw, ["state"]), []);
});

test("missingKeys reports every key when raw isn't an object", () => {
  assert.deepEqual(missingKeys(null, ["id", "title"]), ["id", "title"]);
  assert.deepEqual(missingKeys(undefined, ["id"]), ["id"]);
  assert.deepEqual(missingKeys([1, 2], ["id"]), ["id"]);
});

test("missingKeys can't resolve a nested path through a missing parent", () => {
  const raw = { id: 1 };
  assert.deepEqual(missingKeys(raw, ["author.user.slug"]), ["author.user.slug"]);
});

// ---- missingKeysAcross ----

test("missingKeysAcross returns [] for no samples — nothing to check, not everything missing", () => {
  assert.deepEqual(missingKeysAcross([], ["action", "comment", "commentAnchor"]), []);
});

test("missingKeysAcross counts a key present if any sample has it", () => {
  const samples = [{ action: "COMMENTED", comment: {} }, { action: "COMMENTED", comment: {}, commentAnchor: {} }];
  assert.deepEqual(missingKeysAcross(samples, ["action", "comment", "commentAnchor"]), []);
});

test("missingKeysAcross reports a key missing from every sample", () => {
  const samples = [{ action: "COMMENTED" }, { action: "COMMENTED" }];
  assert.deepEqual(missingKeysAcross(samples, ["action", "comment", "commentAnchor"]), ["comment", "commentAnchor"]);
});

test("missingKeysAcross ignores non-object samples defensively", () => {
  assert.deepEqual(missingKeysAcross([null, undefined, { action: "COMMENTED" }], ["action"]), []);
});

// ---- followPages ----

test("followPages stops at the first isLastPage and passes the right start each call", async () => {
  const calls = [];
  const pages = [
    { values: [1], isLastPage: false, nextPageStart: 25 },
    { values: [2], isLastPage: false, nextPageStart: 50 },
    { values: [3], isLastPage: true },
  ];
  const fetchPage = async (start) => {
    calls.push(start);
    return pages[calls.length - 1];
  };
  const result = await followPages(fetchPage);
  assert.deepEqual(calls, [0, 25, 50]);
  assert.deepEqual(result, { pages, truncated: false });
});

test("followPages returns a single page as-is when it's already last", async () => {
  const page = { values: [1], isLastPage: true };
  const result = await followPages(async () => page);
  assert.deepEqual(result, { pages: [page], truncated: false });
});

test("followPages stops at the cap and reports truncated when isLastPage is never reached", async () => {
  let calls = 0;
  const fetchPage = async (start) => {
    calls += 1;
    return { values: [], isLastPage: false, nextPageStart: start + 25 };
  };
  const result = await followPages(fetchPage, { maxPages: 3 });
  assert.equal(calls, 3);
  assert.equal(result.pages.length, 3);
  assert.equal(result.truncated, true);
});

test("followPages defaults the cap to 20 pages", async () => {
  let calls = 0;
  const fetchPage = async (start) => {
    calls += 1;
    return { values: [], isLastPage: false, nextPageStart: start + 25 };
  };
  const result = await followPages(fetchPage);
  assert.equal(calls, 20);
  assert.equal(result.truncated, true);
});

test("followPages stops with a WARN, keeping the pages so far, when a page isn't an object", async () => {
  const warnings = [];
  const first = { values: [1], isLastPage: false, nextPageStart: 25 };
  const answers = [first, null];
  let calls = 0;
  const result = await followPages(async () => answers[calls++], { warn: (m) => warnings.push(m) });
  assert.equal(calls, 2);
  assert.deepEqual(result, { pages: [first], truncated: false, malformed: true });
  assert.equal(warnings.length, 1);
  assert.match(warnings[0], /not an object/);
});

test("followPages stops with a WARN when isLastPage is false but nextPageStart isn't a number", async () => {
  for (const bad of [undefined, "25", null, NaN]) {
    const warnings = [];
    const page = { values: [1], isLastPage: false, nextPageStart: bad };
    let calls = 0;
    const result = await followPages(
      async () => {
        calls += 1;
        return page;
      },
      { warn: (m) => warnings.push(m) },
    );
    assert.equal(calls, 1, String(bad));
    assert.deepEqual(result, { pages: [page], truncated: false, malformed: true });
    assert.match(warnings[0], /nextPageStart/);
  }
});

// ---- compareVersion ----

test("compareVersion reports no change when nothing was recorded before", () => {
  assert.deepEqual(compareVersion(null, "8.19.1"), { changed: false, message: null });
  assert.deepEqual(compareVersion(undefined, "8.19.1"), { changed: false, message: null });
});

test("compareVersion reports no change when the recorded version matches", () => {
  assert.deepEqual(compareVersion({ version: "8.19.1", seenAt: "2026-01-01T00:00:00.000Z" }, "8.19.1"), {
    changed: false,
    message: null,
  });
});

test("compareVersion flags a change with a clear message", () => {
  assert.deepEqual(compareVersion({ version: "8.19.1" }, "8.20.0"), {
    changed: true,
    message: "Bitbucket changed from 8.19.1 to 8.20.0 — re-check the contract.",
  });
});
