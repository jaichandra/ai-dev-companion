// The Ticket workspace is offered on Jira tickets, pull requests and CI builds, and the ticket
// comes from the page: its key, a PR's branch or title, a build's job name, parameters or commits.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const EXT = path.join(__dirname, "..", "..", "chrome-extension");

function load(fetchImpl) {
  const sandbox = { fetch: fetchImpl, console, URL, Date, Map, RegExp, Promise, Array, String, Set };
  sandbox.self = sandbox;
  vm.createContext(sandbox);
  for (const f of ["registry.js", "target-patterns.js", "jenkins-urls.js", "git-host.js", "features/ticket-workspace.js"]) {
    new vm.Script(fs.readFileSync(path.join(EXT, f), "utf8"), { filename: f }).runInContext(sandbox);
  }
  return vm.runInContext("PaiRegistry.all().find((e) => e.id === 'ticket-workspace')", sandbox);
}

const bitbucket = { sites: {}, git: "bitbucket-dc" };
const ctxFor = (url, service) => {
  const u = new URL(url);
  return { url, origin: u.origin, pathname: u.pathname, search: u.search, targets: bitbucket, service };
};
// The entry runs in its own vm realm: compare plain copies.
const cond = async (entry, ctx) => {
  const r = await entry.condition(ctx);
  return r && JSON.parse(JSON.stringify(r));
};
const json = (body) => ({ ok: true, json: async () => body });

test("it is listed for Jira, git and CI pages, and its pattern matches all three", () => {
  const entry = load(async () => ({ ok: false }));
  assert.deepEqual([...entry.site], ["issues", "git", "ci"]);
  const re = entry.urlPattern(bitbucket);
  assert.ok(re.test("https://jira.example.com/browse/PROJ-7"));
  assert.ok(re.test("https://git.example.com/projects/ACME/repos/app/pull-requests/12/overview"));
  assert.ok(re.test("https://ci.example.com/job/app/job/PROJ-7-login/14/"));
  assert.ok(!re.test("https://jira.example.com/projects/PROJ"));
});

test("on a Jira ticket the key is the page's", async () => {
  const entry = load(async () => assert.fail("no request needed"));
  assert.deepEqual(await cond(entry, ctxFor("https://jira.example.com/browse/PROJ-7")), { issueKey: "PROJ-7" });
});

test("on a pull request the key comes from its branch, then its title; none means no row", async () => {
  const pr = (fromBranch, title) => json({ state: "OPEN", title, fromRef: { displayId: fromBranch }, toRef: { displayId: "master" } });
  const url = (n) => `https://git.example.com/projects/ACME/repos/app/pull-requests/${n}/overview`;
  const asked = [];
  const entry = load(async (u) => (asked.push(u), u.endsWith("/1") ? pr("feature/PROJ-7-login", "Login") : u.endsWith("/2") ? pr("tidy", "[PROJ-9] Tidy") : pr("tidy", "Tidy")));
  assert.deepEqual(await cond(entry, ctxFor(url(1))), { issueKey: "PROJ-7" });
  assert.deepEqual(await cond(entry, ctxFor(url(2))), { issueKey: "PROJ-9" });
  assert.equal(await cond(entry, ctxFor(url(3))), null);
  await cond(entry, ctxFor(url(1)));
  assert.equal(asked.filter((u) => u.endsWith("/1")).length, 1, "the answer is kept for a minute");
});

test("an unreadable pull request is not listed and not remembered", async () => {
  let calls = 0;
  const entry = load(async () => (calls++, { ok: false }));
  const ctx = ctxFor("https://git.example.com/projects/ACME/repos/app/pull-requests/5/overview");
  assert.equal(await cond(entry, ctx), null);
  assert.equal(await cond(entry, ctx), null);
  assert.equal(calls, 2);
});

test("on a build the key comes from the job name, then parameters, then commits", async () => {
  const build = (body) => async (u) => (assert.match(u, /\/api\/json\?tree=description,displayName,actions\[parameters\[name,value\]\],changeSets\[items\[msg\]\]$/), json(body));
  const url = "https://ci.example.com/job/app/job/PROJ-7-login/14/";
  assert.deepEqual(await cond(load(build({})), ctxFor(url)), { issueKey: "PROJ-7" });

  const plain = "https://ci.example.com/job/app/job/main/15/";
  const withParam = build({ actions: [{ parameters: [{ name: "BRANCH", value: "bugfix/PROJ-12-null" }] }, {}] });
  assert.deepEqual(await cond(load(withParam), ctxFor(plain)), { issueKey: "PROJ-12" });
  const withCommit = build({ changeSets: [{ items: [{ msg: "PROJ-30: handle empty" }] }] });
  assert.deepEqual(await cond(load(withCommit), ctxFor(plain)), { issueKey: "PROJ-30" });
  assert.equal(await cond(load(build({ description: "nightly" })), ctxFor("https://ci.example.com/job/app/job/main/16/")), null);
});

test("a build Jenkins won't answer is not listed", async () => {
  const entry = load(async () => { throw new Error("offline"); });
  assert.equal(await cond(entry, ctxFor("https://ci.example.com/job/app/job/main/17/")), null);
});
