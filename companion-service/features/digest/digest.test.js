// The digest from the build (npm run build first), with every Bitbucket and
// Jira read injected — no network — and HOME in a temp dir.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "digest-home-"));
const { computeDigest, createDigestTask, createDigestFeature } = require("../../dist/features/digest/index.js");
const { AuthSetupError } = require("../../dist/core/atlassian.js");
const { jobStore } = require("../../dist/core/jobs.js");
const registry = require("../../core/feature-registry.js");
const { TOOL_CATALOG } = require("../../core/mcp-tools.js");

const NOW = Date.parse("2026-09-29T08:30:00Z");
const pr = (id, over = {}) => ({
  id,
  title: `PR ${id}`,
  state: "OPEN",
  project: "CI",
  repo: "sample-app",
  fromSha: `sha${id}000`,
  url: `https://bb.example/projects/CI/repos/sample-app/pull-requests/${id}`,
  approvals: 0,
  reviewers: [],
  ...over,
});

function io(over = {}) {
  const calls = [];
  return {
    calls,
    io: {
      listDashboardPullRequests: async (auth, role) => {
        calls.push(["dashboard", role]);
        return role === "AUTHOR" ? [pr(1), pr(2, { approvals: 1 })] : [pr(9, { authorSlug: "bob" })];
      },
      getMergeStatus: async (auth, p, r, id) => ({ conflicted: id === 1 }),
      getCommitBuildStatus: async (auth, p, r, sha) => ({ state: sha === "sha1000" ? "FAILED" : "SUCCESSFUL", counts: {}, builds: [] }),
      searchIssues: async (auth, jql) => {
        calls.push(["search", jql]);
        return [{ key: "PROJ-7", summary: "Login broken", status: "Open", priority: "High", url: "https://jira.example/browse/PROJ-7" }];
      },
      jobs: () => [
        { id: "a", featureId: "resolve-conflict", status: "awaiting-approval", startedVia: "watcher", scopeKey: "bitbucket:CI/sample-app#1", createdAt: NOW - 1000, result: { summary: "Resolved 1 file" } },
        { id: "b", featureId: "resolve-conflict", status: "awaiting-approval", startedVia: "extension", createdAt: NOW },
        { id: "c", featureId: "analyze-issue", status: "awaiting-approval", startedVia: "watcher", createdAt: NOW - 5 * 24 * 3600 * 1000 },
      ],
      ...over,
    },
  };
}

test("the digest combines your PRs, your review queue, your tickets and the watchers' results", async () => {
  const f = io();
  const d = await computeDigest({}, f.io, {}, NOW);
  assert.equal(d.headline, "Needs you: 1 conflict, 1 failing build, 1 PR to review, 1 ready for you.");
  assert.deepEqual(d.sections.map((s) => s.id), ["my-prs", "to-review", "tickets", "ready"]);
  assert.equal(d.sections[0].items[0].text, "CI/sample-app #1 PR 1 — conflicts, build failing");
  assert.equal(d.sections[3].items.length, 1, "only watcher jobs from the last 3 days");
  assert.deepEqual(f.calls.filter((c) => c[0] === "dashboard").map((c) => c[1]), ["AUTHOR", "REVIEWER"]);
  assert.match(f.calls.find((c) => c[0] === "search")[1], /^assignee = currentUser\(\) AND statusCategory != Done/);
});

test("a source that fails becomes a note, never a failed digest", async () => {
  const f = io({
    listDashboardPullRequests: async () => {
      throw new AuthSetupError("no session");
    },
    getMergeStatus: async () => {
      throw new Error("x");
    },
  });
  const d = await computeDigest({}, f.io, {}, NOW);
  const notes = d.sections.find((s) => s.id === "notes").items.map((i) => i.text);
  assert.deepEqual(notes, [
    "Couldn't read your pull requests: no sign-in (open the site in Chrome, or save a token in ⚙ Settings).",
    "Couldn't read your review queue: no sign-in (open the site in Chrome, or save a token in ⚙ Settings).",
  ]);
});

test("the scheduled digest posts one inbox item and takes over what quiet hours held", async () => {
  const added = [];
  const task = createDigestTask({ config: {}, io: io().io, notifications: { add: (i) => added.push(i), absorbPending: () => ["x", "y"] } });
  const report = await task({ now: () => NOW });
  assert.equal(added.length, 1);
  assert.equal(added[0].kind, "digest");
  assert.match(added[0].key, /^digest:2026-09-29$/);
  assert.equal(report.absorbed, 2);
  assert.equal(report.conflicts, 1);
});

test("the feature is a read-only polled job with a get_digest tool, registered and in the catalog", async () => {
  const feature = createDigestFeature({}, {}, io().io);
  const job = await feature.start({}, { auth: {} });
  assert.equal(job.featureId, "digest");
  for (let i = 0; i < 20 && jobStore.get(job.id).status === "running"; i++) await new Promise((r) => setImmediate(r));
  const done = jobStore.get(job.id);
  assert.equal(done.status, "awaiting-approval");
  assert.match(done.result.summary, /^Needs you:/);
  assert.ok(done.data.digest.sections.length > 0);
  await assert.rejects(feature.approve(done, { auth: {} }), /read-only/);
  await feature.reject(done, { auth: {} });
  assert.equal(jobStore.get(job.id).status, "rejected");
  assert.deepEqual(feature.mcpTools().map((t) => t.name), ["get_digest"]);
  assert.ok(registry.allFeatureIds().includes("digest"));
  const def = TOOL_CATALOG.find((d) => d.name === "get_digest");
  assert.equal(def.featureId, "digest");
  assert.equal(def.kind, "read");
});

const CONFIG = { bitbucket: { baseUrl: "https://bb.example" }, jira: { baseUrl: "https://jira.example" } };

test("links must be https on the configured host; others are dropped from the digest", async () => {
  const f = io({
    listDashboardPullRequests: async (auth, role) => (role === "AUTHOR" ? [pr(1, { url: "http://bb.example/x" }), pr(2, { url: "https://evil.example/y" })] : [pr(9, { url: "javascript:alert(1)" })]),
    getMergeStatus: async () => ({ conflicted: true }),
    searchIssues: async () => [{ key: "PROJ-7", summary: "x", status: "Open", priority: "High", url: "https://evil.example/browse/PROJ-7" }],
  });
  const d = await computeDigest(CONFIG, f.io, {}, NOW);
  const urls = d.sections.flatMap((s) => s.items.map((i) => i.url));
  assert.deepEqual(urls.filter(Boolean), [], "nothing off-host or non-https survives");
  const ok = await computeDigest(CONFIG, io().io, {}, NOW);
  assert.ok(ok.sections.flatMap((s) => s.items.map((i) => i.url)).includes("https://jira.example/browse/PROJ-7"));
});

test("error text in notes and in watcher-job lines is redacted", async () => {
  const f = io({
    listDashboardPullRequests: async () => {
      throw new Error("GET https://u:secret@bb/x failed: Bearer abc123");
    },
    jobs: () => [{ id: "a", featureId: "analyze-issue", status: "failed", startedVia: "watcher", createdAt: NOW - 1000, error: "token=Bearer zzz999 rejected" }],
  });
  const d = await computeDigest(CONFIG, f.io, {}, NOW);
  const text = JSON.stringify(d);
  assert.doesNotMatch(text, /secret|abc123|zzz999/);
});

test("per-PR merge and build failures become notes; only 10 of your PRs are looked at; a failed watcher job shows as failed", async () => {
  const many = Array.from({ length: 14 }, (_, i) => pr(i + 1));
  let merges = 0;
  const f = io({
    listDashboardPullRequests: async (auth, role) => (role === "AUTHOR" ? many : []),
    getMergeStatus: async (auth, p, r, id) => {
      merges++;
      if (id === 1) throw new Error("boom");
      return { conflicted: false };
    },
    getCommitBuildStatus: async (auth, p, r, sha) => {
      if (sha === "sha2000" || sha === "sha3000") throw new Error("boom");
      return null;
    },
    jobs: () => [{ id: "a", featureId: "resolve-conflict", status: "failed", startedVia: "watcher", scopeKey: "bitbucket:CI/sample-app#1", createdAt: NOW - 1000, error: "no clone" }],
  });
  const d = await computeDigest(CONFIG, f.io, {}, NOW);
  assert.equal(merges, 10);
  const notes = d.sections.find((s) => s.id === "notes").items.map((i) => i.text);
  assert.deepEqual(notes, ["Couldn't check 1 of your PRs for conflicts.", "Couldn't read the builds of 2 of your PRs."]);
  const ready = d.sections.find((s) => s.id === "ready");
  assert.equal(ready.items[0].tone, "bad");
  assert.match(ready.items[0].text, /failed/);
});

test("a digest that throws inside the feature fails the job with a redacted error; the digest job is never persisted", async () => {
  const feature = createDigestFeature(CONFIG, {}, io({ jobs: () => {
    throw new Error("disk Bearer abc123 broke");
  } }).io);
  const job = await feature.start({}, { auth: {} });
  for (let i = 0; i < 20 && jobStore.get(job.id).status === "running"; i++) await new Promise((r) => setImmediate(r));
  const done = jobStore.get(job.id);
  assert.equal(done.status, "failed");
  assert.doesNotMatch(done.error, /abc123/);
  assert.equal(require("../../core/job-files.js").featureIdsToPersist(["digest", "analyze-issue"]).includes("digest"), false);
});

test("upgrading turns the digest on for an existing install (default-on) without touching others", () => {
  const before = registry.allFeatureIds().filter((id) => id !== "digest");
  const migrated = registry.migrateEnabledFeatures({ enabledFeatures: ["analyze-issue"], knownFeatures: before });
  assert.ok(migrated.enabledFeatures.includes("digest"));
  assert.ok(migrated.enabledFeatures.includes("analyze-issue"));
  const unchecked = registry.migrateEnabledFeatures({ enabledFeatures: ["analyze-issue", "digest"], knownFeatures: registry.allFeatureIds() });
  assert.equal(unchecked, null, "a known feature the user left as is stays as is");
});
