const test = require("node:test");
const assert = require("node:assert/strict");
const { resolveLocation, repoKeyFromPageUrl } = require("./location-resolve.js");

const repos = () => [
  {
    key: "ACME/sample-app",
    root: "/Users/me/sample-app",
    files: [
      "src/app/login.ts",
      "src/app/index.ts",
      "src/components/Button.tsx",
      "tests/login.spec.ts",
      "src/main/java/com/acme/ci/LoginPage.java",
    ],
  },
  {
    key: "ACME/sample-service",
    root: "/Users/me/sample-service",
    files: ["src/app/index.ts", "src/billing/invoice.ts"],
  },
];
const loc = (p, line = 10, column = 5) => ({ path: p, line, column });

test("an absolute path inside a configured repo resolves directly", () => {
  const r = resolveLocation(loc("/Users/me/sample-app/src/app/login.ts"), repos());
  assert.deepEqual(r, {
    ok: true,
    repoKey: "ACME/sample-app",
    root: "/Users/me/sample-app",
    rel: "src/app/login.ts",
    abs: "/Users/me/sample-app/src/app/login.ts",
    line: 10,
    column: 5,
  });
});

test("a build-machine absolute path matches by its longest tracked suffix", () => {
  const r = resolveLocation(loc("/home/ci/workspace/sample-app/src/app/login.ts", 42, 13), repos());
  assert.equal(r.ok, true);
  assert.equal(r.abs, "/Users/me/sample-app/src/app/login.ts");
  assert.equal(r.line, 42);
});

test("webpack-relative and Java package paths resolve", () => {
  assert.equal(resolveLocation(loc("src/components/Button.tsx"), repos()).rel, "src/components/Button.tsx");
  assert.equal(
    resolveLocation(loc("com/acme/ci/LoginPage.java", 88, null), repos()).rel,
    "src/main/java/com/acme/ci/LoginPage.java",
  );
});

test("a file name found in two repos is ambiguous, unless a hint picks the repo", () => {
  const ambiguous = resolveLocation(loc("src/app/index.ts"), repos());
  assert.equal(ambiguous.ok, false);
  assert.equal(ambiguous.reason, "ambiguous");
  assert.deepEqual(ambiguous.matches, [
    { repoKey: "ACME/sample-app", rel: "src/app/index.ts" },
    { repoKey: "ACME/sample-service", rel: "src/app/index.ts" },
  ]);
  const hinted = resolveLocation(loc("src/app/index.ts"), repos(), { repoKey: "ACME/sample-service" });
  assert.equal(hinted.ok, true);
  assert.equal(hinted.abs, "/Users/me/sample-service/src/app/index.ts");
});

test("the longest matching suffix wins, and a bare file name shared by two repos stays ambiguous", () => {
  const r = resolveLocation(loc("/ci/sample-app/src/app/index.ts"), repos());
  assert.equal(r.ok, false); // "sample-app/src/app/index.ts" matches nothing, "src/app/index.ts" matches both repos
  assert.equal(r.reason, "ambiguous");
  const r2 = resolveLocation(loc("billing/src/billing/invoice.ts"), repos());
  assert.equal(r2.ok, true);
  assert.equal(r2.repoKey, "ACME/sample-service");
});

test("an unknown file, an empty path and a traversal attempt are not found", () => {
  assert.deepEqual(resolveLocation(loc("src/nope/missing.ts"), repos()), { ok: false, reason: "not-found" });
  assert.deepEqual(resolveLocation(loc("/"), repos()), { ok: false, reason: "not-found" });
  assert.deepEqual(resolveLocation(loc("../../etc/passwd.py"), repos()), { ok: false, reason: "not-found" });
});

test("only tracked files can match, even when the path is inside a repo root", () => {
  const r = resolveLocation(loc("/Users/me/sample-app/node_modules/x/evil.js"), repos());
  assert.deepEqual(r, { ok: false, reason: "not-found" });
});

test("an absolute path that only looks like a repo root prefix does not match it", () => {
  // Without the "root + /" boundary, "/r/appother/src/a.ts" would strip "/r/app" and resolve
  // to the tracked "other/src/a.ts". With it, the path falls through to suffix matching,
  // where "src/a.ts" fits two tracked files and is ambiguous.
  const repo = { key: "P/app", root: "/r/app", files: ["other/src/a.ts", "src/a.ts"] };
  const r = resolveLocation(loc("/r/appother/src/a.ts"), [repo]);
  assert.equal(r.ok, false);
  assert.equal(r.reason, "ambiguous");
  // and the genuine prefix still resolves directly
  assert.equal(resolveLocation(loc("/r/app/other/src/a.ts"), [repo]).rel, "other/src/a.ts");
});

test("repoKeyFromPageUrl finds the configured repo of a Bitbucket page, case-insensitively", () => {
  const keys = ["ACME/sample-app", "ACME/sample-service"];
  assert.equal(repoKeyFromPageUrl("https://bb.example/projects/ACME/repos/sample-app/pull-requests/12/diff", keys), "ACME/sample-app");
  assert.equal(repoKeyFromPageUrl("https://bb.example/projects/acme/repos/Sample-Service/browse?at=x", keys), "ACME/sample-service");
  assert.equal(repoKeyFromPageUrl("https://bb.example/projects/OTHER/repos/x/browse", keys), undefined);
  assert.equal(repoKeyFromPageUrl("https://jenkins.example/job/x/5/", keys), undefined);
  assert.equal(repoKeyFromPageUrl("https://bb.example/projects/%E0%A4%A/repos/x", keys), undefined);
  assert.equal(repoKeyFromPageUrl(undefined, keys), undefined);
});
