// core/providers.ts from the build (npm run build first): which implementation the
// profile selects, that config is read live, and what the Jira provider sends —
// global fetch stubbed (no network) and HOME in a temp dir first.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "providers-home-"));
const providers = require("../dist/core/providers.js");
// The build's own copy: the provider module reads this one, not the source file beside it.
const environment = require("../dist/environment.js");

const config = () => ({
  jira: { baseUrl: "https://jira.example.com/" },
  bitbucket: { baseUrl: "https://bb.example.com" },
  jenkins: { baseUrl: "https://ci.example.com" },
});

test("the profile's sites pick the Bitbucket DC, Jira DC and Jenkins implementations", () => {
  const p = providers.createProviders(config());
  assert.equal(p.git.id, "bitbucket-dc");
  assert.equal(p.issues.id, "jira-dc");
  assert.equal(p.ci.id, "jenkins");
  for (const site of environment.sites) assert.ok(providers.PROVIDER_FACTORIES[site.kind][site.provider], site.id);
});

test("a base URL saved in config applies on the next call, without rebuilding the provider", () => {
  const cfg = config();
  const p = providers.providersFor(cfg);
  assert.equal(p.git.baseUrl(), "https://bb.example.com");
  cfg.bitbucket.baseUrl = "https://other.example.com";
  assert.equal(p.git.baseUrl(), "https://other.example.com");
});

test("providersFor builds once per config object", () => {
  const cfg = config();
  assert.equal(providers.providersFor(cfg), providers.providersFor(cfg));
  assert.notEqual(providers.providersFor(cfg), providers.providersFor(config()));
});

test("web addresses come from the provider, not from the caller", () => {
  const p = providers.providersFor(config());
  assert.equal(p.git.prUrl("PROJ", "repo", 7), "https://bb.example.com/projects/PROJ/repos/repo/pull-requests/7");
  assert.equal(p.issues.issueUrl("PROJ-1"), "https://jira.example.com/browse/PROJ-1");
});

test("capabilities say what each provider offers", () => {
  const p = providers.providersFor(config());
  assert.ok(p.git.capabilities.has("defaultReviewers"));
  assert.ok(p.issues.capabilities.has("subtasks"));
  assert.deepEqual([...p.issues.subtaskParentTypes].sort(), [...environment.issues.subtaskParentTypes].sort());
});

test("a profile naming a provider that doesn't exist fails with the known ones listed", () => {
  const git = environment.sites.find((s) => s.kind === "git");
  const was = git.provider;
  git.provider = "nope";
  try {
    assert.throws(() => providers.createProviders(config()), /No git provider named "nope" \(known: bitbucket-dc, github\)/);
  } finally {
    git.provider = was;
  }
});

test("createSubtask asks Jira for a subtask of the profile's type with the assignee as {name}", async () => {
  const real = globalThis.fetch;
  const sent = [];
  globalThis.fetch = async (url, init) => {
    sent.push({ url: String(url), method: init?.method, body: init?.body ? JSON.parse(init.body) : undefined });
    return new Response(JSON.stringify({ key: "PROJ-2" }), { status: 201, headers: { "content-type": "application/json" } });
  };
  try {
    const cfg = { ...config(), jira: { baseUrl: "https://jira.example.com" } };
    const auth = { cookie: "JSESSIONID=x", origin: "https://jira.example.com" };
    const out = await providers.providersFor(cfg).issues.createSubtask(auth, { projectKey: "PROJ", parentKey: "PROJ-1", summary: "s", assignee: "alice" });
    assert.deepEqual(out, { key: "PROJ-2" });
    assert.equal(sent.length, 1);
    assert.equal(sent[0].url, "https://jira.example.com/rest/api/2/issue");
    assert.deepEqual(sent[0].body.fields, {
      project: { key: "PROJ" },
      parent: { key: "PROJ-1" },
      summary: "s",
      issuetype: { name: environment.issues.subtaskTypeName },
      assignee: { name: "alice" },
    });
  } finally {
    globalThis.fetch = real;
  }
});
