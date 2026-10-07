// The three PR-page features' registration and condition(), loaded as Chrome loads them (shared scripts, then the
// feature files), run against a Bitbucket page and a GitHub page with fetch and the companion stubbed.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const EXT = path.join(__dirname, "..", "..", "chrome-extension");

function load() {
  const sandbox = { console, fetch: async () => ({ ok: false }) };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  vm.createContext(sandbox);
  for (const script of ["target-patterns.js", "registry.js", "ui-kit.js", "git-host.js", "features/resolve-conflict.js", "features/address-review-comments.js", "features/review-in-editor.js"]) {
    vm.runInContext(fs.readFileSync(path.join(EXT, script), "utf8"), sandbox, { filename: script });
  }
  const entries = Object.fromEntries([...vm.runInContext("PaiRegistry.all()", sandbox)].map((e) => [e.id, e]));
  return { sandbox, entries };
}

const BB = "https://bb.example.com";
const GH = "https://github.com";
const bbTargets = { providers: { git: "bitbucket-dc" } };
const ghTargets = { providers: { git: "github" } };
const bbUrl = `${BB}/projects/ACME/repos/sample-app/pull-requests/12/overview`;
const ghUrl = `${GH}/Octo/hello/pull/7/files`;

/** What content.js does for an entry on a page: its pattern against the url, then condition with the match. */
async function run(entry, { url, origin, targets, service, fetchImpl }, sandbox) {
  const pattern = typeof entry.urlPattern === "function" ? entry.urlPattern(targets) : entry.urlPattern;
  const match = url.match(pattern);
  if (!match) return { matched: false };
  if (fetchImpl) sandbox.fetch = fetchImpl;
  return { matched: true, payload: await entry.condition({ url, origin, targets, match, service }), match };
}

const status = (over = {}) => ({ state: "OPEN", conflicted: true, isFork: false, fromBranch: "fix/login", toBranch: "main", title: "Fix login", commentCount: 4, openTaskCount: null, ...over });
const bbFetch = ({ conflicted = true, pr = {} } = {}) => async (url) => ({
  ok: true,
  json: async () => (url.endsWith("/merge") ? { conflicted } : { state: "OPEN", title: "Fix login", fromRef: { displayId: "fix/login" }, toRef: { displayId: "master" }, ...pr }),
});

test("all three register for the git site, use the profile's PR address pattern, and name the git settings group", () => {
  const { entries } = load();
  for (const id of ["resolve-conflict", "address-review-comments", "review-in-editor"]) {
    assert.equal(entries[id].site, "git", id);
    assert.equal(typeof entries[id].urlPattern, "function", `${id}: the pattern depends on the host`);
  }
  assert.deepEqual([...entries["resolve-conflict"].settingsGroups], ["git"]);
  assert.deepEqual([...entries["address-review-comments"].settingsGroups], ["git"]);
});

test("each matches its own host's PR pages and no others", async () => {
  const { sandbox, entries } = load();
  for (const entry of Object.values(entries)) {
    assert.equal((await run(entry, { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch() }, sandbox)).matched, true);
    assert.equal((await run(entry, { url: ghUrl, origin: GH, targets: bbTargets }, sandbox)).matched, false, "a GitHub address is not a Bitbucket PR");
    assert.equal((await run(entry, { url: ghUrl, origin: GH, targets: ghTargets, service: async () => ({ status: status() }) }, sandbox)).matched, true);
    assert.equal((await run(entry, { url: bbUrl, origin: BB, targets: ghTargets }, sandbox)).matched, false, "and the other way round");
  }
});

// ---- resolve-conflict ----

test("resolve-conflict, Bitbucket: listed for a conflicted PR with the branches it reads", async () => {
  const { sandbox, entries } = load();
  const out = await run(entries["resolve-conflict"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch() }, sandbox);
  assert.deepEqual({ ...out.payload }, { project: "ACME", repo: "sample-app", prId: 12, sourceBranch: "fix/login", destBranch: "master" });
});

test("resolve-conflict, Bitbucket: not listed when the PR doesn't conflict or can't be read", async () => {
  const { sandbox, entries } = load();
  assert.equal((await run(entries["resolve-conflict"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch({ conflicted: false }) }, sandbox)).payload, null);
  assert.equal((await run(entries["resolve-conflict"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: async () => ({ ok: false }) }, sandbox)).payload, null);
});

test("resolve-conflict, GitHub: listed for a conflicted PR, from what the companion says", async () => {
  const { sandbox, entries } = load();
  const out = await run(entries["resolve-conflict"], { url: ghUrl, origin: GH, targets: ghTargets, service: async () => ({ status: status() }) }, sandbox);
  assert.deepEqual({ ...out.payload }, { project: "Octo", repo: "hello", prId: 7, sourceBranch: "fix/login", destBranch: "main" });
});

test("resolve-conflict, GitHub: not listed when it doesn't conflict, when that's unknown, or for a fork", async () => {
  const { sandbox, entries } = load();
  for (const over of [{ conflicted: false }, { conflicted: null }, { isFork: true }]) {
    const out = await run(entries["resolve-conflict"], { url: ghUrl, origin: GH, targets: ghTargets, service: async () => ({ status: status(over) }) }, sandbox);
    assert.equal(out.payload, null, JSON.stringify(over));
  }
});

// ---- address-review-comments ----

test("address-review-comments: listed when there may be comments, hidden when there are none or the PR is closed", async () => {
  const { sandbox, entries } = load();
  const entry = entries["address-review-comments"];
  const github = (over) => run(entry, { url: ghUrl, origin: GH, targets: ghTargets, service: async () => ({ status: status(over) }) }, sandbox);
  assert.deepEqual({ ...(await github({ commentCount: 4 })).payload }, { project: "Octo", repo: "hello", prId: 7 });
  assert.deepEqual({ ...(await github({ commentCount: null, openTaskCount: null })).payload }, { project: "Octo", repo: "hello", prId: 7 }, "unknown counts: show it anyway");
  assert.equal((await github({ commentCount: 0, openTaskCount: 0 })).payload, null);
  assert.equal((await github({ state: "MERGED" })).payload, null);
  assert.equal((await github({ isFork: true })).payload, null);
});

test("address-review-comments, Bitbucket: the single-PR answer without `properties` still lists the row", async () => {
  const { sandbox, entries } = load();
  const out = await run(entries["address-review-comments"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch() }, sandbox);
  assert.deepEqual({ ...out.payload }, { project: "ACME", repo: "sample-app", prId: 12 });
  const none = await run(entries["address-review-comments"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch({ pr: { properties: { commentCount: 0, openTaskCount: 0 } } }) }, sandbox);
  assert.equal(none.payload, null);
  const declined = await run(entries["address-review-comments"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch({ pr: { state: "DECLINED" } }) }, sandbox);
  assert.equal(declined.payload, null);
});

// ---- review-in-editor ----

test("review-in-editor: the PR's branches, title and own address, on either host", async () => {
  const { sandbox, entries } = load();
  const bb = await run(entries["review-in-editor"], { url: bbUrl, origin: BB, targets: bbTargets, fetchImpl: bbFetch({ conflicted: false }) }, sandbox);
  assert.deepEqual({ ...bb.payload }, { project: "ACME", repo: "sample-app", prId: 12, sourceBranch: "fix/login", targetBranch: "master", title: "Fix login", prUrl: `${BB}/projects/ACME/repos/sample-app/pull-requests/12` });
  const gh = await run(entries["review-in-editor"], { url: ghUrl, origin: GH, targets: ghTargets, service: async () => ({ status: status({ conflicted: false }) }) }, sandbox);
  assert.deepEqual({ ...gh.payload }, { project: "Octo", repo: "hello", prId: 7, sourceBranch: "fix/login", targetBranch: "main", title: "Fix login", prUrl: `${GH}/Octo/hello/pull/7` });
});

test("review-in-editor: not listed for a fork, or when the source branch is unknown", async () => {
  const { sandbox, entries } = load();
  for (const over of [{ isFork: true }, { fromBranch: null }]) {
    const out = await run(entries["review-in-editor"], { url: ghUrl, origin: GH, targets: ghTargets, service: async () => ({ status: status(over) }) }, sandbox);
    assert.equal(out.payload, null, JSON.stringify(over));
  }
});

// ---- scope keys ----

test("resolve-conflict's scope key follows the host once targets are known", async () => {
  const { sandbox, entries } = load();
  const bb = entries["resolve-conflict"].scopeKey(bbUrl.match(entries["resolve-conflict"].urlPattern(bbTargets)));
  assert.equal(bb, "bitbucket:ACME/sample-app#12");
  vm.runInContext("PaiGit.configure({ providers: { git: 'github' } })", sandbox);
  const gh = entries["resolve-conflict"].scopeKey(ghUrl.match(entries["resolve-conflict"].urlPattern(ghTargets)));
  assert.equal(gh, "github:octo/hello#7");
});
