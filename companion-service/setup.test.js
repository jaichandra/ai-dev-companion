const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const {
  renderExtensionConfig,
  loadExistingConfig,
  parseNumberList,
  discoverRepos,
  nonInteractiveConfig,
  promptFeatureSelection,
  resolvePrerequisites,
  collectFeatureConfigs,
  select,
  waitForExtension,
  migrateApiTokens,
} = require("./setup.js");
const { allFeatureIds, defaultEnabledFeatureIds } = require("./core/feature-registry.js");
const { createCredentialStore } = require("./core/credential-store.js");
const { ensureMcpToken } = require("./core/mcp-registration.js");
const { writeConfigFile } = require("./core/settings.js");

/** Real (but network-free) git repo: `git init` + a fake origin remote —
 * same shape core/prereqs.test.js uses, needed here because
 * discoverRepos skips a path that isn't an actual git clone. */
function makeFakeClone(dir, originUrl) {
  fs.mkdirSync(dir, { recursive: true });
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["remote", "add", "origin", originUrl], { cwd: dir });
}

/** A fake readline interface: `question(prompt, cb)` answers from a fixed
 * queue in order, ignoring the prompt text itself — this is what lets
 * the wizard's prompts (built around real rl.question calls) run headless
 * in a test. Throws if asked for more answers than were scripted, so a
 * test that gets the interaction wrong fails loudly instead of hanging. */
function scriptedRl(answers) {
  let i = 0;
  return {
    question(_prompt, cb) {
      if (i >= answers.length) {
        throw new Error(`scriptedRl: asked a question but ran out of scripted answers (asked ${i + 1})`);
      }
      cb(answers[i++]);
    },
  };
}

function tmpRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "setup-test-"));
}

/** A real credential-store.js instance rooted entirely under a fresh temp
 * dir — never the real companion-service/credentials.enc or
 * ~/.ai-dev-companion/credentials.key — for migrateApiTokens' tests. */
function tempCredentialStore() {
  const dir = tmpRoot();
  return {
    dir,
    store: createCredentialStore({
      filePath: path.join(dir, "credentials.enc"),
      keyPath: path.join(dir, "credentials.key"),
    }),
  };
}

test("renderExtensionConfig sets COMPANION_CONFIG with the service URL and secret", () => {
  const source = renderExtensionConfig("new-secret", 9999);
  const self = {};
  new Function("self", source)(self);
  assert.deepEqual(self.COMPANION_CONFIG, {
    serviceBaseUrl: "http://127.0.0.1:9999",
    sharedSecret: "new-secret",
    contextMenuMatches: [],
  });
});

test("renderExtensionConfig carries the hosts of the right-click menu", () => {
  const self = {};
  new Function("self", renderExtensionConfig("s", 1, ["https://git.example.com/*"]))(self);
  assert.deepEqual(self.COMPANION_CONFIG.contextMenuMatches, ["https://git.example.com/*"]);
});

test("loadExistingConfig returns null when the file does not exist", () => {
  const dir = tmpRoot();
  const result = loadExistingConfig(path.join(dir, "config.json"));
  assert.equal(result, null);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("loadExistingConfig returns null for malformed JSON", () => {
  const dir = tmpRoot();
  const configPath = path.join(dir, "config.json");
  fs.writeFileSync(configPath, "{not valid json");
  const result = loadExistingConfig(configPath);
  assert.equal(result, null);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("loadExistingConfig returns the parsed config when it has port + sharedSecret", () => {
  const dir = tmpRoot();
  const configPath = path.join(dir, "config.json");
  fs.writeFileSync(configPath, JSON.stringify({ port: 8787, sharedSecret: "abc", repos: {} }));
  const result = loadExistingConfig(configPath);
  assert.equal(result.port, 8787);
  assert.equal(result.sharedSecret, "abc");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("parseNumberList accepts all/none and comma- or space-separated numbers", () => {
  assert.deepEqual(parseNumberList("all", 3), [0, 1, 2]);
  assert.deepEqual(parseNumberList("NONE", 3), []);
  assert.deepEqual(parseNumberList("3,1", 3), [0, 2]);
  assert.deepEqual(parseNumberList(" 1  2 ", 3), [0, 1]);
  assert.deepEqual(parseNumberList("2,2", 3), [1]);
});

test("parseNumberList rejects out-of-range or non-numeric answers", () => {
  assert.equal(parseNumberList("4", 3), null);
  assert.equal(parseNumberList("0", 3), null);
  assert.equal(parseNumberList("yes", 3), null);
  assert.equal(parseNumberList("", 3), null);
});

test("discoverRepos returns the existing repos unchanged when nothing is found", () => {
  const existing = { "OTHER/repo": "/some/path" };
  assert.deepEqual(discoverRepos(existing, []), { repos: existing, added: [] });
});

test("discoverRepos adds every discovered clone keyed from its origin, without asking", () => {
  const root = tmpRoot();
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
  makeFakeClone(path.join(root, "billing"), "ssh://git@bitbucket.example.com:7999/acme/sample-service.git");
  makeFakeClone(path.join(root, "mirror"), "https://github.com/some/mirror.git");
  fs.mkdirSync(path.join(root, "not-a-repo"));

  const existing = { "OTHER/repo": "/some/path" };
  const { repos, added } = discoverRepos(existing, [root]);
  assert.deepEqual(repos, {
    "OTHER/repo": "/some/path",
    "ACME/sample-service": path.join(root, "billing"),
    "ACME/sample-app": path.join(root, "sample-app"),
  });
  assert.deepEqual(added, ["ACME/sample-app", "ACME/sample-service"]);
  assert.deepEqual(existing, { "OTHER/repo": "/some/path" }, "doesn't mutate its input");

  fs.rmSync(root, { recursive: true, force: true });
});

test("discoverRepos keeps an existing mapping rather than re-adding the key or path", () => {
  const root = tmpRoot();
  makeFakeClone(path.join(root, "sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
  makeFakeClone(path.join(root, "sample-app-copy"), "https://bitbucket.example.com/scm/acme/sample-app.git");

  const existing = { "acme/Sample-App": path.join(root, "sample-app-copy") };
  assert.deepEqual(discoverRepos(existing, [root]), { repos: existing, added: [] });

  fs.rmSync(root, { recursive: true, force: true });
});

test("promptFeatureSelection: a blank answer keeps every currently enabled feature", async () => {
  const rl = scriptedRl([""]);
  const result = await promptFeatureSelection(rl, allFeatureIds());
  assert.deepEqual(result, allFeatureIds());
});

test("promptFeatureSelection: a blank answer keeps nothing when nothing was enabled", async () => {
  const rl = scriptedRl([""]);
  const result = await promptFeatureSelection(rl, []);
  assert.deepEqual(result, []);
});

test("promptFeatureSelection enables exactly the numbers given, in registry order", async () => {
  const rl = scriptedRl(["3,1"]);
  const result = await promptFeatureSelection(rl, []);
  assert.deepEqual(result, [allFeatureIds()[0], allFeatureIds()[2]]);
});

test("promptFeatureSelection re-asks on an invalid answer, and can end with none", async () => {
  const rl = scriptedRl(["99", "none"]);
  const result = await promptFeatureSelection(rl, allFeatureIds());
  assert.deepEqual(result, []);
});

const passing = () => ({ ok: true, message: "fine" });
const failing = () => ({ ok: false, message: "broken" });

test("resolvePrerequisites asks nothing when every check passes", async () => {
  const rl = scriptedRl([]);
  const checks = { git: passing, claudeCli: passing, claudeAuth: passing };
  const result = await resolvePrerequisites(rl, allFeatureIds(), checks);
  assert.deepEqual(result, allFeatureIds());
});

test("resolvePrerequisites re-checks on Enter until the check passes", async () => {
  let calls = 0;
  const flaky = () => (++calls < 3 ? failing() : passing());
  const rl = scriptedRl(["", ""]);
  const checks = { git: passing, claudeCli: passing, claudeAuth: flaky };
  const result = await resolvePrerequisites(rl, ["resolve-conflict"], checks);
  assert.deepEqual(result, ["resolve-conflict"]);
  assert.equal(calls, 3);
});

test("resolvePrerequisites 'skip' drops only the features that need the failing check", async () => {
  const rl = scriptedRl(["skip"]);
  const checks = { git: passing, claudeCli: failing, claudeAuth: failing };
  const result = await resolvePrerequisites(
    rl,
    ["resolve-conflict", "review-in-editor", "create-jira-subtasks"],
    checks,
  );
  // claudeAuth is never checked once resolve-conflict (its only user) is skipped.
  assert.deepEqual(result, ["review-in-editor", "create-jira-subtasks"]);
});

test("collectFeatureConfigs never touches rl when nothing is selected", async () => {
  const rl = scriptedRl([]); // throws if asked anything at all
  const result = await collectFeatureConfigs(rl, [], [], {});
  assert.deepEqual(result, {});
});

test("collectFeatureConfigs never asks about a feature with no promptSetup (resolve-conflict)", async () => {
  const rl = scriptedRl([]); // throws if asked anything at all
  assert.deepEqual(await collectFeatureConfigs(rl, ["resolve-conflict"], [], {}), {});
  assert.deepEqual(await collectFeatureConfigs(rl, ["resolve-conflict"], ["resolve-conflict"], {}), {});
});

test("collectFeatureConfigs always prompts a newly-enabled feature (no 'change?' gate)", async () => {
  const rl = scriptedRl([""]); // analyze-issue's Claude model: blank -> Claude Code's default
  const result = await collectFeatureConfigs(rl, ["analyze-issue"], [], { jira: { baseUrl: "https://jira.example.com" } });
  assert.equal(result.analyzeIssue.model, undefined);
});

test("collectFeatureConfigs skips an already-enabled feature that declines the change prompt", async () => {
  const rl = scriptedRl(["n"]);
  const result = await collectFeatureConfigs(rl, ["create-jira-subtasks"], ["create-jira-subtasks"], {});
  assert.deepEqual(result, {});
});


test("collectFeatureConfigs prompts review-in-editor and defaults its answer", async () => {
  // A reviewEditor is set on `existing` so the default doesn't depend on what's actually
  // installed on whatever machine runs this test (see core/prereqs.js's detectInstalledEditors).
  const rl = scriptedRl([""]);
  const result = await collectFeatureConfigs(rl, ["review-in-editor"], [], { reviewEditor: "cursor" });
  assert.deepEqual(result, { reviewEditor: "cursor" });
});

test("collectFeatureConfigs handles a mix: one newly enabled, one already-enabled-and-declined", async () => {
  // create-jira-subtasks is gated -> "n"; review-in-editor is new -> straight into its prompt (blank).
  const rl = scriptedRl(["n", ""]);
  const existing = { reviewEditor: "cursor", jira: { baseUrl: "https://jira.example.com", apiToken: "tok" } };
  const result = await collectFeatureConfigs(
    rl,
    ["create-jira-subtasks", "review-in-editor"],
    ["create-jira-subtasks"],
    existing,
  );
  assert.deepEqual(result, { reviewEditor: "cursor" });
});

test("select returns the option chosen by number", async () => {
  const rl = scriptedRl(["2"]);
  const result = await select(
    rl,
    "Pick one",
    [
      { id: "a", label: "A" },
      { id: "b", label: "B" },
    ],
    "a",
  );
  assert.equal(result, "b");
});

test("select returns the option chosen by typing its id", async () => {
  const rl = scriptedRl(["b"]);
  const result = await select(
    rl,
    "Pick one",
    [
      { id: "a", label: "A" },
      { id: "b", label: "B" },
    ],
    "a",
  );
  assert.equal(result, "b");
});

test("select falls back to the default on a blank answer", async () => {
  const rl = scriptedRl([""]);
  const result = await select(
    rl,
    "Pick one",
    [
      { id: "a", label: "A" },
      { id: "b", label: "B" },
    ],
    "b",
  );
  assert.equal(result, "b");
});

test("select re-prompts on an invalid answer before accepting a good one", async () => {
  const rl = scriptedRl(["nope", "1"]);
  const result = await select(
    rl,
    "Pick one",
    [
      { id: "a", label: "A" },
      { id: "b", label: "B" },
    ],
    "a",
  );
  assert.equal(result, "a");
});

test("nonInteractiveConfig keeps settings, adds new repos, and soft-migrates enabledFeatures", () => {
  const existing = {
    port: 8788,
    sharedSecret: "s",
    enabledFeatures: ["review-in-editor"],
    reviewEditor: "cursor",
    repos: { "ACME/sample-app": "/w" },
  };
  const discover = (repos) => ({ repos: { ...repos, "ACME/new": "/n" }, added: ["ACME/new"] });
  const result = nonInteractiveConfig(existing, discover);
  assert.equal(result.port, 8788);
  assert.equal(result.sharedSecret, "s");
  assert.equal(result.reviewEditor, "cursor");
  assert.deepEqual(result.repos, { "ACME/sample-app": "/w", "ACME/new": "/n" });
  // Legacy list without knownFeatures → every default-on feature.
  assert.deepEqual(result.enabledFeatures, defaultEnabledFeatureIds());
  assert.deepEqual(result.knownFeatures, allFeatureIds());
  assert.ok(result.enabledFeatures.includes("analyze-issue"));
  assert.equal(nonInteractiveConfig(null, discover), null);
});

test("nonInteractiveConfig auto-enables newly shipped default-on features on update", () => {
  const prior = allFeatureIds().filter((id) => id !== "analyze-issue");
  const existing = {
    port: 8787,
    sharedSecret: "s",
    enabledFeatures: ["review-in-editor"],
    knownFeatures: prior,
    repos: { "ACME/sample-app": "/w" },
  };
  const discover = (repos) => ({ repos, added: [] });
  const result = nonInteractiveConfig(existing, discover);
  assert.ok(result.knownFeatures.includes("analyze-issue"));
  assert.ok(result.enabledFeatures.includes("analyze-issue"));
  assert.ok(result.enabledFeatures.includes("review-in-editor"));
});

test("nonInteractiveConfig leaves repos alone when no enabled feature uses them", () => {
  const all = require("./core/feature-registry.js").allFeatureIds();
  const existing = {
    port: 1,
    sharedSecret: "s",
    // Only create-jira-subtasks (no needsRepos) — and knownFeatures already
    // records every id so soft-migrate doesn't re-enable the others.
    enabledFeatures: ["create-jira-subtasks"],
    knownFeatures: all,
    repos: {},
  };
  const discover = () => assert.fail("shouldn't look for repos");
  assert.deepEqual(nonInteractiveConfig(existing, discover), existing);
});

/** A readline stand-in for waitForExtension: holds the pending question so
 * a test can "press Enter", and records whether it was aborted. */
function pendingRl() {
  const rl = { answer: null, aborted: false };
  rl.question = (_prompt, { signal }, cb) => {
    rl.answer = () => cb("");
    signal.addEventListener("abort", () => (rl.aborted = true));
  };
  return rl;
}

test("waitForExtension resolves once the extension says hello, and drops the prompt", async () => {
  const rl = pendingRl();
  const replies = [{ connected: false }, { connected: false }, { connected: true, version: "0.4.2" }];
  let polls = 0;
  const result = await waitForExtension(rl, async () => replies[polls++], { intervalMs: 1 });
  assert.deepEqual(result, { connected: true, version: "0.4.2", skipped: false });
  assert.equal(polls, 3);
  assert.equal(rl.aborted, true);
});

test("waitForExtension stops polling when the user presses Enter", async () => {
  const rl = pendingRl();
  let polls = 0;
  const waiting = waitForExtension(
    rl,
    async () => {
      polls++;
      return { connected: false };
    },
    { intervalMs: 5 },
  );
  rl.answer();
  assert.deepEqual(await waiting, { connected: false, version: null, skipped: true });
  const pollsAtSkip = polls;
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(polls, pollsAtSkip);
});

test("waitForExtension gives up after the timeout", async () => {
  const rl = pendingRl();
  const result = await waitForExtension(rl, async () => ({ connected: false }), {
    intervalMs: 1,
    timeoutMs: 20,
  });
  assert.deepEqual(result, { connected: false, version: null, skipped: false });
});

test("migrateApiTokens moves a freshly-typed apiToken into the store and blanks it in config", () => {
  const { dir, store } = tempCredentialStore();
  try {
    const config = { jira: { baseUrl: "https://jira.example.com", apiToken: "typed-in-wizard" } };
    migrateApiTokens(config, store);
    assert.equal(config.jira.apiToken, undefined);
    assert.equal(store.get("jira.apiToken"), "typed-in-wizard");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("migrateApiTokens is a no-op when there's nothing to migrate", () => {
  const { dir, store } = tempCredentialStore();
  try {
    const config = { jira: { baseUrl: "https://jira.example.com" } };
    migrateApiTokens(config, store);
    assert.deepEqual(config, { jira: { baseUrl: "https://jira.example.com" } });
    assert.deepEqual(store.list(), []);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("migrateApiTokens plus token creation never writes mcp.token into config.json", () => {
  const { dir, store } = tempCredentialStore();
  try {
    const config = { port: 8787, jira: { baseUrl: "https://jira.example.com", apiToken: "typed-in-wizard" } };
    migrateApiTokens(config, store);
    const { token, created } = ensureMcpToken(store);
    assert.equal(created, true);
    assert.ok(token.length > 0);

    const configPath = path.join(dir, "config.json");
    writeConfigFile(configPath, config);
    const written = fs.readFileSync(configPath, "utf8");
    assert.ok(!written.includes(token), "config.json must never carry the mcp token");
    assert.ok(!written.includes("mcp.token"), "config.json must never carry the mcp.token credential name");
    assert.equal(config.jira.apiToken, undefined);
    assert.equal(store.get("mcp.token"), token);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
