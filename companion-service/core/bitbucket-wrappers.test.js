// core/bitbucket.ts's ticket wrappers from the build (npm run build first),
// with global fetch stubbed — no network — and HOME in a temp dir first.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "bb-wrappers-home-"));
const bb = require("../dist/core/bitbucket.js");
const e = require("./bitbucket-endpoints.js");

const SITE = { baseUrl: "https://bb.example", label: "Bitbucket", configKey: "bitbucket.apiToken", apiToken: "tok" };

/** Runs `fn` with fetch answering from `answers` (path substring -> [status, body]); returns the requested paths. */
async function withFetch(answers, fn) {
  const real = globalThis.fetch;
  const seen = [];
  globalThis.fetch = async (url) => {
    const u = String(url).replace("https://bb.example", "");
    seen.push(u);
    const hit = Object.entries(answers).find(([k]) => u.includes(k));
    const [status, body] = hit ? hit[1] : [404, { errors: [] }];
    return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
  };
  try {
    return { result: await fn(), seen };
  } finally {
    globalThis.fetch = real;
  }
}

test("getDefaultBranch: falls to the second endpoint on 404, null when both 404, throws on any other status", async () => {
  const second = await withFetch({ "/branches/default": [200, { id: "refs/heads/main" }] }, () => bb.getDefaultBranch(SITE, {}, "ACME", "sample-app"));
  assert.equal(second.result, "main");
  assert.equal(second.seen.length, 2);
  const none = await withFetch({}, () => bb.getDefaultBranch(SITE, {}, "ACME", "sample-app"));
  assert.equal(none.result, null);
  await assert.rejects(withFetch({ "/default-branch": [500, { errors: [{ message: "boom" }] }] }, () => bb.getDefaultBranch(SITE, {}, "ACME", "sample-app")), /boom|500/);
});

test("getCommitBuildStatus: null on 404, reads the build-status endpoint, throws on other statuses", async () => {
  const sha = "abc1234";
  const none = await withFetch({}, () => bb.getCommitBuildStatus(SITE, {}, "ACME", "sample-app", sha));
  assert.equal(none.result, null);
  const plugin = await withFetch(
    { "/rest/build-status/1.0/": [200, { values: [{ state: "SUCCESSFUL", key: "k", name: "n", url: "https://ci/1", dateAdded: 1 }] }] },
    () => bb.getCommitBuildStatus(SITE, {}, "ACME", "sample-app", sha),
  );
  assert.equal(plugin.result.state, "SUCCESSFUL");
  await assert.rejects(withFetch({ "/build-status/": [503, {}] }, () => bb.getCommitBuildStatus(SITE, {}, "ACME", "sample-app", sha)), /503|failed/);
});

test("createPullRequest refuses an unsafe branch or repo before anything is sent", async () => {
  const good = { project: "ACME", repo: "sample-app", title: "t", description: "", fromBranch: "bugfix/PROJ-7-x", toBranch: "master", reviewers: [] };
  for (const bad of [{ fromBranch: "a;b" }, { toBranch: "--x" }, { repo: "../x" }, { project: "a b" }]) {
    const sent = await withFetch({}, async () => {
      await assert.rejects(bb.createPullRequest(SITE, {}, { ...good, ...bad }));
    });
    assert.deepEqual(sent.seen, []);
  }
  assert.doesNotThrow(() => e.createPullRequestBody(good));
  for (const bad of [{ fromBranch: "a;b" }, { toBranch: "a..b" }]) assert.throws(() => e.createPullRequestBody({ ...good, ...bad }));
});
