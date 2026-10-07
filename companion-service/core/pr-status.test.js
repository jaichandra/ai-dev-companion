const test = require("node:test");
const assert = require("node:assert/strict");
const { createPrStatus, PrStatusRequestError, isFork } = require("./pr-status.js");

const PR = {
  id: 42,
  state: "OPEN",
  title: "Fix login",
  fromBranch: "fix/login",
  toBranch: "main",
  fromRepo: { projectKey: "Octo", slug: "hello" },
  toRepo: { projectKey: "octo", slug: "Hello" },
  commentCount: 8,
};

function fakeGit(overrides = {}) {
  const calls = [];
  const git = {
    capabilities: new Set(["mergeStatus"]),
    prUrl: (p, r, n) => `https://git.example.com/${p}/${r}/pull/${n}`,
    async getPullRequest(auth, p, r, n) {
      calls.push(["getPullRequest", p, r, n]);
      return PR;
    },
    async getMergeStatus(auth, p, r, n) {
      calls.push(["getMergeStatus", p, r, n]);
      return { conflicted: true, canMerge: false, outcome: "DIRTY" };
    },
    ...overrides,
  };
  return { git, calls };
}

test("answers in one host-neutral shape", async () => {
  const { git } = fakeGit();
  const status = createPrStatus({ getGit: () => git });
  assert.deepEqual(await status.get({}, "octo", "hello", 42), {
    state: "OPEN",
    conflicted: true,
    isFork: false,
    fromBranch: "fix/login",
    toBranch: "main",
    title: "Fix login",
    prUrl: "https://git.example.com/octo/hello/pull/42",
    commentCount: 8,
    openTaskCount: null,
  });
});

test("a PR that isn't open is not checked for conflicts", async () => {
  const { git, calls } = fakeGit({ getPullRequest: async () => ({ ...PR, state: "MERGED" }) });
  const out = await createPrStatus({ getGit: () => git }).get({}, "o", "r", 1);
  assert.equal(out.state, "MERGED");
  assert.equal(out.conflicted, null);
  assert.ok(!calls.some((c) => c[0] === "getMergeStatus"));
});

test("a host without merge status, or a merge check that fails, leaves conflicted unknown", async () => {
  const none = fakeGit({ capabilities: new Set() });
  assert.equal((await createPrStatus({ getGit: () => none.git }).get({}, "o", "r", 1)).conflicted, null);
  const broken = fakeGit({ getMergeStatus: async () => { throw new Error("boom"); } });
  assert.equal((await createPrStatus({ getGit: () => broken.git }).get({}, "o", "r", 1)).conflicted, null);
  const unknown = fakeGit({ getMergeStatus: async () => ({ conflicted: null }) });
  assert.equal((await createPrStatus({ getGit: () => unknown.git }).get({}, "o", "r", 1)).conflicted, null);
});

test("isFork: same repo (case-insensitive) is not a fork, a different one is, a missing one is unknown", () => {
  assert.equal(isFork(PR), false);
  assert.equal(isFork({ fromRepo: { projectKey: "me", slug: "hello" }, toRepo: { projectKey: "octo", slug: "hello" } }), true);
  assert.equal(isFork({ fromRepo: { projectKey: null, slug: null }, toRepo: { projectKey: "octo", slug: "hello" } }), null);
  assert.equal(isFork({}), null);
  assert.equal(isFork(null), null);
});

test("the same PR is looked up once within the cache window, and again after it", async () => {
  const { git, calls } = fakeGit();
  let t = 1000;
  const status = createPrStatus({ getGit: () => git, now: () => t, ttlMs: 20_000 });
  await status.get({}, "octo", "hello", 42);
  await status.get({}, "OCTO", "Hello", 42);
  t += 19_000;
  await status.get({}, "octo", "hello", 42);
  assert.equal(calls.filter((c) => c[0] === "getPullRequest").length, 1);
  t += 2_000;
  await status.get({}, "octo", "hello", 42);
  assert.equal(calls.filter((c) => c[0] === "getPullRequest").length, 2);
  await status.get({}, "octo", "hello", 43);
  assert.equal(calls.filter((c) => c[0] === "getPullRequest").length, 3, "another PR is another lookup");
});

test("calls that arrive together share one lookup", async () => {
  const { git, calls } = fakeGit();
  const status = createPrStatus({ getGit: () => git });
  await Promise.all([status.get({}, "o", "r", 1), status.get({}, "o", "r", 1), status.get({}, "o", "r", 1)]);
  assert.equal(calls.filter((c) => c[0] === "getPullRequest").length, 1);
});

test("a failure is remembered for a few seconds only", async () => {
  let fail = true;
  const { git, calls } = fakeGit({
    getPullRequest: async () => {
      calls.push(["getPullRequest"]);
      if (fail) throw new Error("no token");
      return PR;
    },
  });
  let t = 0;
  const status = createPrStatus({ getGit: () => git, now: () => t });
  await assert.rejects(() => status.get({}, "o", "r", 1), /no token/);
  await new Promise((resolve) => setImmediate(resolve));
  await assert.rejects(() => status.get({}, "o", "r", 1), /no token/);
  assert.equal(calls.length, 1, "within the error window it is not asked again");
  fail = false;
  t += 6_000;
  assert.equal((await status.get({}, "o", "r", 1)).state, "OPEN");
  assert.equal(calls.filter((c) => c[0] === "getPullRequest").length, 2);
});

test("a PR the host doesn't return is an error", async () => {
  const { git } = fakeGit({ getPullRequest: async () => null });
  await assert.rejects(() => createPrStatus({ getGit: () => git }).get({}, "o", "r", 5), /didn't return pull request o\/r#5/);
});

test("anything that isn't a plain name and a positive number is refused before a lookup", async () => {
  const { git, calls } = fakeGit();
  const status = createPrStatus({ getGit: () => git });
  for (const [p, r, n] of [["o", "../x", 1], ["o/x", "r", 1], ["", "r", 1], ["o", "r", 0], ["o", "r", -1], ["o", "r", 1.5], ["o", "r", NaN], ["o", "r", "1"]]) {
    await assert.rejects(() => status.get({}, p, r, n), PrStatusRequestError, JSON.stringify([p, r, n]));
  }
  assert.equal(calls.length, 0);
});
