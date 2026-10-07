// What a GitHub distribution gets: run in the tree assembled from the framework and this example profile
// (npm run test:example from the framework's root). Everything here depends on the profile naming a GitHub git site.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "github-profile-home-"));
const environment = require("./environment.js");
const prereqs = require("./core/prereqs.js");
const scopeKey = require("./core/scope-key.js");
const manifest = require("./core/extension-manifest.js");
const settings = require("./core/settings.js");
const historySchema = require("./core/history-schema.js");
const { ALLOWED_NAMES } = require("./core/credential-store.js");
const packs = require("./core/packs.js");
const workspacePlan = require("./features/ticket-workspace/plan.js");
const analyzePlan = require("./features/analyze-issue/plan.js");
const providers = require("./dist/core/providers.js");
const authContext = require("./dist/core/auth-context.js");

test("the profile's git site is GitHub, token-only, and the framework's rules and provider follow it", () => {
  const git = environment.sites.find((s) => s.kind === "git");
  assert.equal(git.id, "github");
  assert.equal(git.provider, "github");
  assert.equal(git.tokenOnly, true);
  assert.equal(prereqs.gitRemoteRules(), prereqs.GIT_REMOTE_RULES.github);
  assert.equal(providers.providersFor({}).git.id, "github");
  assert.equal(providers.providersFor({}).git.prUrl("octo", "hello", 7), "https://github.com/octo/hello/pull/7");
});

test("a GitHub token has a name in the credential store, a Bitbucket one doesn't", () => {
  assert.ok(ALLOWED_NAMES.includes("github.apiToken"));
  assert.ok(!ALLOWED_NAMES.includes("bitbucket.apiToken"));
});

test("clones: origins parse, a fresh clone comes from the page's own host, .git is optional", () => {
  assert.equal(prereqs.deriveCloneUrl({ pageOrigin: "https://github.com", project: "Octo", repo: "hello" }), "https://github.com/Octo/hello.git");
  assert.equal(prereqs.deriveCloneUrl({ pageOrigin: "https://ghe.example.com/", project: "o", repo: ".github" }), "https://ghe.example.com/o/.github.git");
  assert.equal(prereqs.deriveCloneUrl({ existingOrigins: ["git@github.com:me/other.git"], pageOrigin: "https://github.com", project: "Octo", repo: "hello" }), "git@github.com:Octo/hello.git");
  assert.equal(prereqs.originMatchesProjectRepo("https://github.com/Octo/hello", "octo", "hello"), true);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "clones-"));
  fs.mkdirSync(path.join(dir, "hello", ".git"), { recursive: true });
  fs.writeFileSync(path.join(dir, "hello", ".git", "config"), '[remote "origin"]\n\turl = https://github.com/Octo/hello.git\n');
  assert.deepEqual(prereqs.discoverLocalClones([dir]).map((c) => c.key), ["Octo/hello"]);
});

test("pull request keys are github:owner/repo#n, in scope keys, history and watchers", () => {
  assert.equal(scopeKey.scopeKeyFor("resolve-conflict", { project: "Octo", repo: "Hello", prId: 7 }), "github:octo/hello#7");
  assert.equal(scopeKey.scopeKeyFor("address-review-comments", { project: "octo", repo: ".github", prId: "7" }), "github:octo/.github#7");
  assert.equal(scopeKey.isValidScopeKey("github:octo/hello#7"), true);
  assert.equal(historySchema.normalizeKey("Octo/Hello#7"), "github:octo/hello#7");
  assert.equal(require("./core/watchers.js").prKey({ project: "Octo", repo: "Hello", id: 7 }), "github:octo/hello#7");
});

test("PR addresses: GitHub's are links, Bitbucket-shaped ones are not", () => {
  assert.equal(workspacePlan.prUrlAllowed("https://github.com/o/r/pull/3", "https://github.com"), true);
  assert.equal(workspacePlan.prUrlAllowed("https://github.com/o/r/issues/3", "https://github.com"), false);
  assert.equal(workspacePlan.prUrlAllowed("https://evil.example.com/o/r/pull/3", "https://github.com"), false);
  assert.equal(workspacePlan.prUrlAllowed("https://bb.example.com/projects/P/repos/r/pull-requests/3", "https://github.com"), false);
});

test("a ticket that links a GitHub PR or repository is understood", () => {
  const linked = analyzePlan.linkedPullRequests({}, { detail: [{ pullRequests: [{ url: "https://github.com/Octo/hello/pull/7", name: "Fix", status: "OPEN" }] }] }, "https://github.com");
  assert.deepEqual(linked.map((p) => [p.repoKey, p.id, p.url]), [["Octo/hello", 7, "https://github.com/Octo/hello/pull/7"]]);
  const ranked = analyzePlan.candidateReposFromTicket({ fields: { description: "see https://github.com/Octo/hello/blob/main/x.js" } }, { "Octo/hello": "/tmp/hello" }, {});
  assert.equal(ranked[0].repoKey, "Octo/hello");
});

test("the extension: only PR pages on GitHub, no github.com permission (so no cookies), the other sites whole", () => {
  const m = manifest.buildManifest({ name: "x", version: "1", content_scripts: [{ run_at: "document_idle" }] }, {});
  assert.deepEqual(m.host_permissions, ["https://jira.example.com/*", "https://jenkins.example.com/*", "http://127.0.0.1/*"]);
  assert.deepEqual(m.content_scripts[0].matches, ["https://github.com/*/*/pull/*", "https://jira.example.com/*", "https://jenkins.example.com/*"]);
  assert.deepEqual(manifest.contextMenuMatches({}), ["https://github.com/*/*/pull/*", "https://jenkins.example.com/*"]);
});

test("an Enterprise host from config.json moves the PR-page pattern with it", () => {
  const config = { github: { baseUrl: "https://ghe.example.com/" } };
  assert.ok(manifest.matchPatterns(config).includes("https://ghe.example.com/*/*/pull/*"));
  assert.ok(!manifest.matchPatterns(config).some((p) => p.includes("github.com")));
  assert.equal(manifest.hostsChanged({}, config), true, "so saving it regenerates the manifest");
  assert.equal(manifest.hostsChanged({}, {}), false);
});

test("what the extension is told: the git site's origin and provider", () => {
  assert.equal(manifest.originsByKind({}).git, "https://github.com");
  assert.equal(manifest.originsByKind({ github: { baseUrl: "https://ghe.example.com" } }).git, "https://ghe.example.com");
  assert.equal(manifest.providersByKind().git, "github");
});

test("browser sessions are never taken from, or accepted for, the token-only site", () => {
  const origins = authContext.configuredOrigins({});
  assert.deepEqual(origins.sort(), ["https://jenkins.example.com", "https://jira.example.com"]);
  assert.ok(!origins.includes("https://github.com"));
});

test("settings show and accept the GitHub site like any other, with no username", () => {
  const view = settings.publicSettings({ github: { baseUrl: "https://ghe.example.com" } }, { list: () => ["github.apiToken"] });
  assert.deepEqual(view.github, { baseUrl: "https://ghe.example.com", apiTokenSet: true, apiTokenExternal: "" });
  assert.equal(view.bitbucket, undefined);
  assert.deepEqual(settings.validateSettingsUpdate({ github: { baseUrl: "https://ghe.example.com", apiToken: "ghp_x" } }, {}), []);
  const bad = settings.validateSettingsUpdate({ github: { baseUrl: "ftp://nope" } }, {});
  assert.ok(bad.length > 0);
});

test("the framework's features all load against this profile", () => {
  assert.deepEqual(
    packs.features().map((s) => s.descriptor.id),
    ["resolve-conflict", "create-jira-subtasks", "review-in-editor", "diagnose-build", "analyze-issue", "ticket-workspace", "ticket-to-pr", "summarize-comments", "digest", "address-review-comments"],
  );
  for (const [id, factory] of Object.entries(packs.featureFactories())) assert.equal(typeof factory, "function", id);
});

test("the assembled manifest was generated for this profile", () => {
  const generated = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "chrome-extension", "manifest.json"), "utf8"));
  assert.ok(generated.content_scripts[0].matches.includes("https://github.com/*/*/pull/*"));
  assert.ok(!generated.host_permissions.some((h) => h.includes("github.com")));
});
