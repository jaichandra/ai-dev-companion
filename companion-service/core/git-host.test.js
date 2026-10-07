// chrome-extension/git-host.js (PaiGit): how the PR-page features read a pull request on Bitbucket and on GitHub.
const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");

function load() {
  const file = require.resolve("../../chrome-extension/git-host.js");
  delete require.cache[file];
  return require(file);
}

const bitbucketTargets = { providers: { git: "bitbucket-dc" } };
const githubTargets = { providers: { git: "github" } };

// ---- addresses and keys ----

test("a PR page's address: Bitbucket's /projects/P/repos/r/pull-requests/n, GitHub's /owner/repo/pull/n", () => {
  const git = load();
  const bb = git.urlPattern(bitbucketTargets).exec("https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/12/overview");
  assert.deepEqual([bb[1], bb[2], bb[3]], ["ACME", "sample-app", "12"]);
  const gh = git.urlPattern(githubTargets).exec("https://github.com/Octo/hello/pull/7/files");
  assert.deepEqual([gh[1], gh[2], gh[3]], ["Octo", "hello", "7"]);
  assert.ok(git.urlPattern(githubTargets).test("https://ghe.example.com/o/r/pull/9?diff=split"));
  assert.ok(git.urlPattern(githubTargets).test("https://github.com/o/r/pull/9#discussion_r1"));
});

test("GitHub's pattern is not fooled by other pages", () => {
  const re = load().urlPattern(githubTargets);
  for (const url of ["https://github.com/o/r/pulls", "https://github.com/o/r/issues/7", "https://github.com/o/r/pull/x", "https://github.com/o/pull/7", "https://github.com/o/r/pull/7x"]) {
    assert.equal(re.test(url), false, url);
  }
});

test("until /targets answers, and for an unknown provider, the host is Bitbucket", () => {
  const git = load();
  assert.equal(git.urlPattern(undefined), git.URL_PATTERNS["bitbucket-dc"]);
  assert.equal(git.urlPattern({}), git.URL_PATTERNS["bitbucket-dc"]);
  assert.equal(git.urlPattern({ providers: { git: "gitlab" } }), git.URL_PATTERNS["bitbucket-dc"]);
});

test("scope keys are the strings the service makes (core/prereqs.js prKey), by the configured host", () => {
  const git = load();
  assert.equal(git.scopeKey(["", "acme", "Sample-App", "7"]), "bitbucket:ACME/sample-app#7");
  git.configure(githubTargets);
  assert.equal(git.scopeKey(["", "Octo", "Hello", "7"]), "github:octo/hello#7");
  git.configure(bitbucketTargets);
  assert.equal(git.scopeKey(["", "acme", "Sample-App", "7"]), "bitbucket:ACME/sample-app#7");
});

test("the extension's keys equal the service's, for both hosts", () => {
  const git = load();
  const rules = require("./prereqs.js").GIT_REMOTE_RULES;
  for (const [id, prefixed] of [["bitbucket-dc", "bitbucket-dc"], ["github", "github"]]) {
    git.configure({ providers: { git: id } });
    assert.equal(git.scopeKey(["", "Some", "Repo-Name", "42"]), rules[prefixed].prKey("Some", "Repo-Name", 42), id);
  }
});

test("the PR's page address", () => {
  const git = load();
  assert.equal(git.prUrl({ origin: "https://bb.example.com", targets: bitbucketTargets }, { project: "ACME", repo: "sample-app", prId: "7" }), "https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/7");
  assert.equal(git.prUrl({ origin: "https://github.com", targets: githubTargets }, { project: "Octo", repo: "hello", prId: "7" }), "https://github.com/Octo/hello/pull/7");
});

// ---- reading a PR ----

function stubFetch(handler) {
  const calls = [];
  const real = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    calls.push(String(url));
    return handler(String(url), init);
  };
  return { calls, restore: () => (globalThis.fetch = real) };
}
const reply = (body, ok = true) => ({ ok, json: async () => body });

test("Bitbucket: the merge check first; a PR that doesn't conflict stops there", async () => {
  const git = load();
  const stub = stubFetch((url) => reply(url.endsWith("/merge") ? { conflicted: false } : {}));
  try {
    const pr = await git.pr({ origin: "https://bb.example.com", targets: bitbucketTargets }, { project: "ACME", repo: "sample-app", prId: "7" }, { conflicts: true });
    assert.equal(pr.conflicted, false);
    assert.deepEqual(stub.calls, ["https://bb.example.com/rest/api/1.0/projects/ACME/repos/sample-app/pull-requests/7/merge"]);
  } finally {
    stub.restore();
  }
});

test("Bitbucket: a conflicted PR is then read for its branches and title", async () => {
  const git = load();
  const stub = stubFetch((url) =>
    reply(url.endsWith("/merge") ? { conflicted: true } : { state: "OPEN", title: "Fix", fromRef: { displayId: "feat/x" }, toRef: { displayId: "master" }, properties: { commentCount: 3, openTaskCount: 1 } }),
  );
  try {
    const pr = await git.pr({ origin: "https://bb.example.com", targets: bitbucketTargets }, { project: "ACME", repo: "sample-app", prId: "7" }, { conflicts: true, properties: true });
    assert.deepEqual(pr, { state: "OPEN", conflicted: true, isFork: null, fromBranch: "feat/x", toBranch: "master", title: "Fix", commentCount: 3, openTaskCount: 1 });
    assert.match(stub.calls[1], /pull-requests\/7\?withProperties=true$/);
  } finally {
    stub.restore();
  }
});

test("Bitbucket: counts the server doesn't send stay null; a failed read is null, never a throw", async () => {
  const git = load();
  const ctx = { origin: "https://bb.example.com", targets: bitbucketTargets };
  const ids = { project: "ACME", repo: "sample-app", prId: "7" };
  const ok = stubFetch(() => reply({ state: "OPEN", fromRef: { displayId: "a" }, toRef: { displayId: "b" } }));
  try {
    const pr = await git.pr(ctx, ids, {});
    assert.equal(pr.commentCount, null);
    assert.equal(pr.openTaskCount, null);
  } finally {
    ok.restore();
  }
  const notOk = stubFetch(() => reply({}, false));
  try {
    assert.equal(await git.pr(ctx, ids, {}), null);
    assert.equal(await git.pr(ctx, ids, { conflicts: true }), null);
  } finally {
    notOk.restore();
  }
  const broken = stubFetch(() => {
    throw new Error("network");
  });
  try {
    assert.equal(await git.pr(ctx, ids, {}), null);
  } finally {
    broken.restore();
  }
});

test("GitHub: the companion is asked, and no request goes to the page's own origin", async () => {
  const git = load();
  const asked = [];
  const status = { state: "OPEN", conflicted: true, isFork: false, fromBranch: "fix", toBranch: "main", title: "T", commentCount: 2, openTaskCount: null };
  const stub = stubFetch(() => reply({}));
  try {
    const pr = await git.pr(
      { origin: "https://github.com", targets: githubTargets, service: async (type, payload) => (asked.push([type, payload]), { status }) },
      { project: "Octo", repo: "hello", prId: "7" },
      { conflicts: true },
    );
    assert.deepEqual(pr, status);
    assert.deepEqual(asked, [["pr-status", { project: "Octo", repo: "hello", prId: "7" }]]);
    assert.equal(stub.calls.length, 0);
  } finally {
    stub.restore();
  }
});

test("GitHub: a companion that can't answer (no token, not running) is null", async () => {
  const git = load();
  const ctx = (service) => ({ origin: "https://github.com", targets: githubTargets, service });
  const ids = { project: "o", repo: "r", prId: "1" };
  assert.equal(await git.pr(ctx(async () => { throw new Error("no token"); }), ids), null);
  assert.equal(await git.pr(ctx(async () => ({})), ids), null);
  assert.equal(await git.pr(ctx(async () => ({ status: null })), ids), null);
});

// ---- the summary line ----

test("a queued job's summary line reads the payload, whichever host it came from", () => {
  const git = load();
  assert.equal(git.prSummary({ prId: 7, sourceBranch: "a", destBranch: "b" }), "PR #7 · a ← b");
  assert.equal(git.prSummary({ prId: 7 }), "PR #7");
  assert.equal(git.prSummary({ issueKey: "X-1" }), null);
  assert.equal(git.prSummary(undefined), null);
});
