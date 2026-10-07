const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const settings = require("./settings.js");
const registry = require("./feature-registry.js");

const CURRENT = {
  port: 8787,
  sharedSecret: "s3cret",
  repos: { "ACME/sample-app": "/Users/me/gitviews/sample-app" },
  enabledFeatures: ["resolve-conflict", "review-in-editor"],
  knownFeatures: registry.allFeatureIds(),
  reviewEditor: "cursor",
  // apiToken lives in the encrypted credential store now (Task 3), never
  // in config.json — see the fakeCredentials()/fakeStore() helpers below.
  jira: { baseUrl: "https://jira.example.com" },
  jenkins: { baseUrl: "https://jenkins.example.com", username: "me", maxBuildsToScan: 9 },
  analyzeIssue: { model: "claude-opus-5", componentRepoMap: { UI: "ACME/sample-app" } },
  someFutureField: { keep: true },
};

/** Every repo path "exists" and has the origin its key implies, unless
 * overridden — validation's git-backed checks without touching disk. */
function fakeDeps({ check = () => ({ ok: true, message: "OK." }), origins = {} } = {}) {
  return {
    checkRepoPath: check,
    getOriginUrl: (p) => origins[p] ?? `https://bitbucket.example.com/scm/acme/${path.basename(p)}.git`,
  };
}

function fieldsOf(errors) {
  return errors.map((e) => e.field);
}

/** A minimal in-memory stand-in for credential-store.js's real store —
 * just enough of {get, set, remove, list} for migrateTokensToStore's
 * tests, without ever touching a real credentials.enc. */
function fakeStore(initial = {}) {
  const map = { ...initial };
  return {
    get: (name) => map[name],
    set: (name, value) => {
      map[name] = value;
    },
    remove: (name) => {
      delete map[name];
    },
    list: () => Object.keys(map),
  };
}

/** A stand-in for core/credentials.ts's module surface — the `{ setToken,
 * list }` shape publicSettings/mergeSettings take — backed by the same
 * fakeStore() above so a test can assert on what actually got saved. */
function fakeCredentials(initial = {}) {
  const store = fakeStore(initial);
  return {
    setToken: (name, value) => {
      if (value === null) store.remove(name);
      else store.set(name, value);
    },
    list: () => store.list(),
    _store: store,
  };
}

test("publicSettings never exposes the shared secret or any token", () => {
  const credentials = fakeCredentials({ "jira.apiToken": "jira-token", "jenkins.apiToken": "jenkins-token" });
  const view = settings.publicSettings(CURRENT, credentials);
  const text = JSON.stringify(view);
  assert.ok(!text.includes("s3cret"));
  assert.ok(!text.includes("jira-token"));
  assert.ok(!text.includes("jenkins-token"));
  assert.equal(view.sharedSecret, undefined);
  assert.equal(view.jira.apiTokenSet, true);
  assert.equal(view.jenkins.apiTokenSet, true);
  assert.equal(view.port, 8787);
});

test("publicSettings reports no token and blank fields for a minimal config", () => {
  const view = settings.publicSettings({ port: 1, sharedSecret: "x" }, fakeCredentials());
  assert.deepEqual(view.jira, { baseUrl: "", apiTokenSet: false, apiTokenExternal: "" });
  assert.deepEqual(view.bitbucket, { baseUrl: "", apiTokenSet: false, apiTokenExternal: "" });
  assert.deepEqual(view.repos, {});
  assert.equal(view.reviewEditor, null);
  // No enabledFeatures field means "every known feature" (legacy configs).
  assert.deepEqual(view.enabledFeatures, registry.allFeatureIds());
});

test("publicSettings reports apiTokenSet from the credential store, not config", () => {
  // jenkins.apiToken saved in the store but not jira's — publicSettings
  // must ask the store per-name, not just "is anything saved".
  const credentials = fakeCredentials({ "jenkins.apiToken": "jenkins-token" });
  const view = settings.publicSettings(CURRENT, credentials);
  assert.equal(view.jira.apiTokenSet, false);
  assert.equal(view.jenkins.apiTokenSet, true);
  assert.equal(view.bitbucket.apiTokenSet, false);
});

test("publicSettings reports bitbucket's baseUrl and apiTokenSet independently of jira/jenkins", () => {
  const credentials = fakeCredentials({ "bitbucket.apiToken": "bb-token" });
  const view = settings.publicSettings(
    { ...CURRENT, bitbucket: { baseUrl: "https://bitbucket.example.com" } },
    credentials,
  );
  assert.deepEqual(view.bitbucket, { baseUrl: "https://bitbucket.example.com", apiTokenSet: true, apiTokenExternal: "" });
  assert.equal(view.jira.apiTokenSet, false);
});

test("publicSettings names the another tool file a site borrows its token from, only when none is saved", () => {
  const credentials = {
    ...fakeCredentials({ "bitbucket.apiToken": "bb-token" }),
    externalSources: () => ({ jira: "~/.ai/.jira.env", bitbucket: "~/.claude.json (acme-bitbucket)" }),
  };
  const view = settings.publicSettings(CURRENT, credentials);
  assert.equal(view.jira.apiTokenExternal, "~/.ai/.jira.env");
  assert.equal(view.bitbucket.apiTokenExternal, "");
  assert.equal(view.jenkins.apiTokenExternal, "");
});

test("publicSettings reports the sessionCache defaults for a minimal config", () => {
  const view = settings.publicSettings({ port: 1, sharedSecret: "x" }, fakeCredentials());
  assert.deepEqual(view.sessionCache, { ttlMinutes: 30, heartbeat: false });
});

test("publicSettings reports a saved sessionCache", () => {
  const view = settings.publicSettings(
    { port: 1, sharedSecret: "x", sessionCache: { ttlMinutes: 5, heartbeat: true } },
    fakeCredentials(),
  );
  assert.deepEqual(view.sessionCache, { ttlMinutes: 5, heartbeat: true });
});

test("validateSettingsUpdate accepts a full, valid update", () => {
  const errors = settings.validateSettingsUpdate(
    {
      enabledFeatures: ["resolve-conflict", "create-jira-subtasks"],
      repos: { "ACME/sample-app": "/Users/me/gitviews/sample-app", "ACME/sample-service": "/Users/me/gitviews/sample-service" },
      reviewEditor: "cursor",
      jira: { baseUrl: "https://jira.example.com/", apiToken: "", clearApiToken: false },
      jenkins: { baseUrl: "", username: "", apiToken: "abc" },
      bitbucket: { baseUrl: "https://bitbucket.example.com/", apiToken: "bb-tok" },
      analyzeIssue: { model: "" },
      acknowledgeWarnings: true,
    },
    CURRENT,
    fakeDeps(),
  );
  assert.deepEqual(errors, []);
});

test("validateSettingsUpdate refuses port and sharedSecret, and unknown keys", () => {
  const errors = settings.validateSettingsUpdate({ port: 9999, sharedSecret: "new", bogus: 1 }, CURRENT, fakeDeps());
  assert.deepEqual(fieldsOf(errors).sort(), ["bogus", "port", "sharedSecret"]);
  assert.match(errors.find((e) => e.field === "port").message, /npm run setup/);
});

test("validateSettingsUpdate rejects a non-object body", () => {
  assert.equal(settings.validateSettingsUpdate(null, CURRENT).length, 1);
  assert.equal(settings.validateSettingsUpdate([], CURRENT).length, 1);
});

test("validateSettingsUpdate rejects unknown and duplicate feature ids", () => {
  const errors = settings.validateSettingsUpdate(
    { enabledFeatures: ["resolve-conflict", "resolve-conflict", "nope"] },
    CURRENT,
    fakeDeps(),
  );
  assert.equal(errors.length, 2);
  assert.ok(errors.some((e) => /"nope"/.test(e.message)));
  assert.ok(errors.some((e) => /more than once/.test(e.message)));
  assert.equal(settings.validateSettingsUpdate({ enabledFeatures: "all" }, CURRENT).length, 1);
});

test("validateSettingsUpdate rejects unsafe repo keys and relative paths", () => {
  const errors = settings.validateSettingsUpdate(
    {
      repos: {
        "ACME/../etc": "/tmp/x",
        "no-slash": "/tmp/x",
        "ACME/a/b": "/tmp/x",
        "ACME/relative": "gitviews/relative",
        "ACME/blank": "  ",
      },
    },
    CURRENT,
    fakeDeps(),
  );
  assert.deepEqual(fieldsOf(errors).sort(), [
    "repos.ACME/../etc",
    "repos.ACME/a/b",
    "repos.ACME/blank",
    "repos.ACME/relative",
    "repos.no-slash",
  ]);
});

test("validateSettingsUpdate rejects case-insensitive duplicate repo keys", () => {
  const errors = settings.validateSettingsUpdate(
    { repos: { "ACME/sample-app": "/Users/me/gitviews/sample-app", "acme/sample-app": "/Users/me/gitviews/sample-app" } },
    CURRENT,
    fakeDeps(),
  );
  assert.deepEqual(fieldsOf(errors), ["repos.acme/sample-app"]);
});

test("validateSettingsUpdate checks only new or moved repo mappings", () => {
  const checked = [];
  const deps = fakeDeps({
    check: (p) => {
      checked.push(p);
      return { ok: false, message: `${p} does not exist.` };
    },
  });
  const errors = settings.validateSettingsUpdate(
    { repos: { "ACME/sample-app": "/Users/me/gitviews/sample-app", "ACME/new": "/Users/me/gitviews/new" } },
    CURRENT,
    deps,
  );
  assert.deepEqual(checked, ["/Users/me/gitviews/new"]);
  assert.deepEqual(errors, [{ field: "repos.ACME/new", message: "/Users/me/gitviews/new does not exist." }]);
});

test("validateSettingsUpdate rejects a clone whose origin is a different repo", () => {
  const errors = settings.validateSettingsUpdate(
    { repos: { "ACME/other": "/Users/me/gitviews/sample-app2" } },
    CURRENT,
    fakeDeps({ origins: { "/Users/me/gitviews/sample-app2": "https://bitbucket.example.com/scm/acme/sample-app.git" } }),
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0].message, /not ACME\/other/);
});

test("validateSettingsUpdate validates editor, models, URLs and tokens", () => {
  const errors = settings.validateSettingsUpdate(
    {
      reviewEditor: "emacs",
      analyzeIssue: { model: "rm -rf /", extra: 1 },
      jira: { baseUrl: "javascript:alert(1)", apiToken: "has space", clearApiToken: "yes" },
      jenkins: { baseUrl: "ftp://jenkins", username: "a\nb", surprise: true },
      bitbucket: { baseUrl: "not a url", username: "shouldn't be allowed" },
    },
    CURRENT,
    fakeDeps(),
  );
  assert.deepEqual(fieldsOf(errors).sort(), [
    "analyzeIssue.extra",
    "analyzeIssue.model",
    "bitbucket.baseUrl",
    "bitbucket.username",
    "jenkins.baseUrl",
    "jenkins.surprise",
    "jenkins.username",
    "jira.apiToken",
    "jira.baseUrl",
    "jira.clearApiToken",
    "reviewEditor",
  ]);
});

test("validateSettingsUpdate allows clearing reviewEditor with null", () => {
  assert.deepEqual(settings.validateSettingsUpdate({ reviewEditor: null }, CURRENT, fakeDeps()), []);
});

test("validateSettingsUpdate accepts a valid sessionCache", () => {
  assert.deepEqual(
    settings.validateSettingsUpdate({ sessionCache: { ttlMinutes: 30, heartbeat: true } }, CURRENT, fakeDeps()),
    [],
  );
});

test("validateSettingsUpdate rejects an out-of-range or non-integer sessionCache.ttlMinutes", () => {
  for (const bad of [-1, 1.5, "30", 1441]) {
    const errors = settings.validateSettingsUpdate({ sessionCache: { ttlMinutes: bad } }, CURRENT, fakeDeps());
    assert.deepEqual(fieldsOf(errors), ["sessionCache.ttlMinutes"], `ttlMinutes ${JSON.stringify(bad)} should be rejected`);
  }
});

test("validateSettingsUpdate rejects a non-boolean sessionCache.heartbeat", () => {
  const errors = settings.validateSettingsUpdate({ sessionCache: { heartbeat: "yes" } }, CURRENT, fakeDeps());
  assert.deepEqual(fieldsOf(errors), ["sessionCache.heartbeat"]);
});

test("validateSettingsUpdate rejects an unknown sessionCache key, and a non-object sessionCache", () => {
  const unknownKey = settings.validateSettingsUpdate({ sessionCache: { cookie: "a=b" } }, CURRENT, fakeDeps());
  assert.deepEqual(fieldsOf(unknownKey), ["sessionCache.cookie"]);
  assert.match(unknownKey[0].message, /isn't a known setting/);

  const notObject = settings.validateSettingsUpdate({ sessionCache: "off" }, CURRENT, fakeDeps());
  assert.deepEqual(fieldsOf(notObject), ["sessionCache"]);
});

test("mergeSettings keeps tokens, secret, port and unknown fields unless told otherwise, and reports no tokenOps for a blank apiToken", () => {
  const { config: next, tokenOps } = settings.mergeSettings(CURRENT, {
    jira: { baseUrl: "https://jira2.example.com/", apiToken: "" },
    jenkins: { baseUrl: "https://jenkins.example.com", username: "me" },
  });
  assert.equal(next.sharedSecret, "s3cret");
  assert.equal(next.port, 8787);
  assert.deepEqual(next.someFutureField, { keep: true });
  // A blank apiToken in the update means "keep the saved one" — and the
  // token is never part of the config object at all, blank or not.
  assert.deepEqual(next.jira, { baseUrl: "https://jira2.example.com" });
  assert.equal(next.jenkins.apiToken, undefined);
  assert.deepEqual(tokenOps, [], "a blank apiToken is not a token operation");
  assert.equal(next.jenkins.maxBuildsToScan, 9);
  // The input is never mutated.
  assert.equal(CURRENT.jira.baseUrl, "https://jira.example.com");
});

test("mergeSettings reports a token set/clear as a pending tokenOp, and never applies it itself", () => {
  const { config: replaced, tokenOps: setOps } = settings.mergeSettings(CURRENT, {
    jira: { apiToken: " new-token " },
  });
  assert.equal(replaced.jira.apiToken, undefined);
  assert.deepEqual(setOps, [{ name: "jira.apiToken", value: "new-token" }]);

  const { config: cleared, tokenOps: clearOps } = settings.mergeSettings(CURRENT, {
    jira: { apiToken: "ignored", clearApiToken: true },
  });
  assert.equal(cleared.jira.apiToken, undefined);
  assert.equal(cleared.jira.baseUrl, "https://jira.example.com");
  assert.deepEqual(clearOps, [{ name: "jira.apiToken", value: null }]);
});

test("mergeSettings strips a stale apiToken already sitting in config when the store already has one", () => {
  const legacy = { ...CURRENT, jira: { baseUrl: "https://jira.example.com", apiToken: "leftover-from-before" } };
  const { config: next, tokenOps } = settings.mergeSettings(
    legacy,
    { jira: { baseUrl: "https://jira.example.com" } },
    { savedTokenNames: ["jira.apiToken"] },
  );
  assert.equal(next.jira.apiToken, undefined);
  assert.deepEqual(tokenOps, []);
});

// Final-review 5g: a legacy token left in config.json (its migration into
// the store failed) used to be deleted by ANY save that touched the site
// without a new token — losing the only copy.
test("mergeSettings moves a legacy config token into the store (tokenOp) when the store lacks it, then blanks it", () => {
  const legacy = { ...CURRENT, jira: { baseUrl: "https://jira.example.com", apiToken: " leftover " } };
  const { config: next, tokenOps } = settings.mergeSettings(
    legacy,
    { jira: { baseUrl: "https://jira.example.com" } },
    { savedTokenNames: [] },
  );
  assert.equal(next.jira.apiToken, undefined);
  assert.deepEqual(tokenOps, [{ name: "jira.apiToken", value: "leftover" }]);
});

test("mergeSettings: a new token or a clear wins over a legacy config token", () => {
  const legacy = { ...CURRENT, jira: { baseUrl: "https://jira.example.com", apiToken: "leftover" } };
  const setNew = settings.mergeSettings(legacy, { jira: { apiToken: "fresh" } }, { savedTokenNames: [] });
  assert.equal(setNew.config.jira.apiToken, undefined);
  assert.deepEqual(setNew.tokenOps, [{ name: "jira.apiToken", value: "fresh" }]);
  const clear = settings.mergeSettings(legacy, { jira: { clearApiToken: true } }, { savedTokenNames: [] });
  assert.equal(clear.config.jira.apiToken, undefined);
  assert.deepEqual(clear.tokenOps, [{ name: "jira.apiToken", value: null }]);
});

test("mergeSettings keeps a legacy config token untouched when it can't tell whether the store has one", () => {
  const legacy = { ...CURRENT, jira: { baseUrl: "https://jira.example.com", apiToken: "leftover" } };
  const { config: next, tokenOps } = settings.mergeSettings(legacy, { jira: { baseUrl: "https://jira.example.com" } });
  assert.equal(next.jira.apiToken, "leftover");
  assert.deepEqual(tokenOps, []);
  // Blank legacy values are just dropped either way.
  const blank = { ...CURRENT, jira: { baseUrl: "https://jira.example.com", apiToken: "  " } };
  assert.equal(settings.mergeSettings(blank, { jira: {} }).config.jira.apiToken, undefined);
});

test("mergeSettings stays pure: it never mutates current", () => {
  const legacy = { ...CURRENT, jira: { baseUrl: "https://jira.example.com", apiToken: "leftover" } };
  const before = JSON.stringify(legacy);
  settings.mergeSettings(legacy, { jira: { baseUrl: "https://x.example.com" } }, { savedTokenNames: [] });
  assert.equal(JSON.stringify(legacy), before);
});

test("mergeSettings drops blank site fields, and the whole object once empty", () => {
  const { config: next } = settings.mergeSettings(
    { port: 1, sharedSecret: "x", jira: { baseUrl: "https://jira.example.com" } },
    { jira: { baseUrl: " " } },
  );
  assert.equal(next.jira, undefined);
});

test("mergeSettings handles bitbucket like jira/jenkins: baseUrl merge, tokenOps, and dropping once empty", () => {
  const { config: withUrl, tokenOps: setOps } = settings.mergeSettings(CURRENT, {
    bitbucket: { baseUrl: "https://bitbucket.example.com/", apiToken: " bb-token " },
  });
  assert.deepEqual(withUrl.bitbucket, { baseUrl: "https://bitbucket.example.com" });
  assert.deepEqual(setOps, [{ name: "bitbucket.apiToken", value: "bb-token" }]);

  const { config: cleared, tokenOps: clearOps } = settings.mergeSettings(withUrl, {
    bitbucket: { apiToken: "ignored", clearApiToken: true },
  });
  assert.deepEqual(cleared.bitbucket, { baseUrl: "https://bitbucket.example.com" });
  assert.deepEqual(clearOps, [{ name: "bitbucket.apiToken", value: null }]);

  const { config: dropped } = settings.mergeSettings(cleared, { bitbucket: { baseUrl: " " } });
  assert.equal(dropped.bitbucket, undefined);
});

test("mergeSettings orders enabled features like the registry and snapshots knownFeatures", () => {
  const { config: next } = settings.mergeSettings(CURRENT, { enabledFeatures: ["review-in-editor", "resolve-conflict"] });
  assert.deepEqual(next.enabledFeatures, ["resolve-conflict", "review-in-editor"]);
  assert.deepEqual(next.knownFeatures, registry.allFeatureIds());
});

test("mergeSettings replaces repos with resolved paths, and clears reviewEditor/analyzeIssue.model", () => {
  const { config: next } = settings.mergeSettings(CURRENT, {
    repos: { "ACME/new": " /Users/me/gitviews/new/ " },
    reviewEditor: null,
    analyzeIssue: { model: "" },
  });
  assert.deepEqual(next.repos, { "ACME/new": "/Users/me/gitviews/new" });
  assert.equal(next.reviewEditor, undefined);
  assert.deepEqual(next.analyzeIssue, { componentRepoMap: { UI: "ACME/sample-app" } });
  assert.equal("claudeModel" in next, false);
});

test("mergeSettings removes sessionCache entirely when both fields are the defaults", () => {
  const { config: next } = settings.mergeSettings(CURRENT, { sessionCache: { ttlMinutes: 30, heartbeat: false } });
  assert.equal(next.sessionCache, undefined);
});

test("mergeSettings keeps the existing ttlMinutes when only heartbeat is updated", () => {
  const withTtl = { ...CURRENT, sessionCache: { ttlMinutes: 45 } };
  const { config: next } = settings.mergeSettings(withTtl, { sessionCache: { heartbeat: true } });
  assert.deepEqual(next.sessionCache, { ttlMinutes: 45, heartbeat: true });
});

test("mergeSettings persists sessionCache.ttlMinutes: 0 (the off value is meaningful)", () => {
  const { config: next } = settings.mergeSettings(CURRENT, { sessionCache: { ttlMinutes: 0 } });
  assert.deepEqual(next.sessionCache, { ttlMinutes: 0 });
});

// ---- mcp ----

test("publicSettings reports mcp's defaults, and tokenSet from the store without the value", () => {
  const unset = settings.publicSettings({ port: 1, sharedSecret: "x" }, fakeCredentials());
  assert.deepEqual(unset.mcp, { tokenSet: false });

  const view = settings.publicSettings(
    { port: 1, sharedSecret: "x" },
    fakeCredentials({ "mcp.token": "mcp-secret-token" }),
  );
  assert.deepEqual(view.mcp, { tokenSet: true });
  assert.ok(!JSON.stringify(view).includes("mcp-secret-token"));
});

test("validateSettingsUpdate accepts rotateToken: true", () => {
  assert.deepEqual(settings.validateSettingsUpdate({ mcp: { rotateToken: true } }, CURRENT, fakeDeps()), []);
});

test("validateSettingsUpdate rejects a non-true rotateToken, unknown mcp keys and a non-object mcp", () => {
  for (const bad of [false, "true", 1]) {
    const errors = settings.validateSettingsUpdate({ mcp: { rotateToken: bad } }, CURRENT, fakeDeps());
    assert.deepEqual(fieldsOf(errors), ["mcp.rotateToken"], `rotateToken ${JSON.stringify(bad)} should be rejected`);
  }
  const unknownKey = settings.validateSettingsUpdate({ mcp: { token: "abc" } }, CURRENT, fakeDeps());
  assert.deepEqual(fieldsOf(unknownKey), ["mcp.token"]);
  assert.ok(!JSON.stringify(unknownKey).includes("abc"));
  const notObject = settings.validateSettingsUpdate({ mcp: "never" }, CURRENT, fakeDeps());
  assert.deepEqual(fieldsOf(notObject), ["mcp"]);
});

test("mergeSettings turns rotateToken into an mcp.token tokenOp from newMcpToken(), never into config", () => {
  const { config: next, tokenOps } = settings.mergeSettings(
    CURRENT,
    { mcp: { rotateToken: true } },
    { savedTokenNames: [], newMcpToken: () => "fresh-mcp-token" },
  );
  assert.deepEqual(tokenOps, [{ name: "mcp.token", value: "fresh-mcp-token" }]);
  assert.equal(next.mcp, undefined);
  assert.ok(!JSON.stringify(next).includes("rotateToken"));
  assert.ok(!JSON.stringify(next).includes("fresh-mcp-token"));
});

test("mergeSettings never asks for a new MCP token unless rotateToken is set", () => {
  const { tokenOps } = settings.mergeSettings(
    CURRENT,
    { mcp: {} },
    {
      newMcpToken: () => {
        throw new Error("should not be called");
      },
    },
  );
  assert.deepEqual(tokenOps, []);
});

// ---- applyTokenOps ----

test("applyTokenOps applies every op in order and reports ok", () => {
  const applied = [];
  const setToken = (name, value) => applied.push({ name, value });
  const result = settings.applyTokenOps(
    [
      { name: "jira.apiToken", value: "jira-tok" },
      { name: "jenkins.apiToken", value: null },
    ],
    setToken,
  );
  assert.deepEqual(result, { ok: true });
  assert.deepEqual(applied, [
    { name: "jira.apiToken", value: "jira-tok" },
    { name: "jenkins.apiToken", value: null },
  ]);
});

test("applyTokenOps stops at the first failure — ops before it stay applied, and the failure is reported", () => {
  const applied = [];
  const boom = new Error("disk full");
  const setToken = (name, value) => {
    if (name === "jenkins.apiToken") throw boom;
    applied.push({ name, value });
  };
  const result = settings.applyTokenOps(
    [
      { name: "jira.apiToken", value: "jira-tok" },
      { name: "jenkins.apiToken", value: "jenkins-tok" },
      { name: "bitbucket.apiToken", value: "never-reached" },
    ],
    setToken,
  );
  assert.deepEqual(applied, [{ name: "jira.apiToken", value: "jira-tok" }], "the op before the failure stayed applied");
  assert.equal(result.ok, false);
  assert.deepEqual(result.failedOp, { name: "jenkins.apiToken", value: "jenkins-tok" });
  assert.equal(result.error, boom);
});

test("applyTokenOps on an empty list is a no-op that reports ok", () => {
  const setToken = () => assert.fail("should not be called");
  assert.deepEqual(settings.applyTokenOps([], setToken), { ok: true });
});

test("prerequisiteWarnings lists failing checks with the features needing them", () => {
  const checks = {
    git: () => ({ ok: true, message: "git OK." }),
    claudeCli: () => ({ ok: false, message: "claude CLI not found." }),
    claudeAuth: () => ({ ok: false, message: "not logged in." }),
  };
  const warnings = settings.prerequisiteWarnings(["resolve-conflict", "review-in-editor"], checks);
  assert.deepEqual(
    warnings.map((w) => w.check),
    ["claudeCli", "claudeAuth"],
  );
  assert.deepEqual(warnings[0].features, ["Resolve Merge Conflicts"]);
  assert.deepEqual(settings.prerequisiteWarnings(["review-in-editor"], checks), []);
});

test("runAllChecks runs every check some feature requires", () => {
  const checks = {
    git: () => ({ ok: true, message: "a" }),
    claudeCli: () => ({ ok: true, message: "b" }),
    claudeAuth: () => ({ ok: false, message: "c" }),
  };
  assert.deepEqual(Object.keys(settings.runAllChecks(checks)).sort(), ["claudeAuth", "claudeCli", "git"]);
});

test("pendingRestart only cares about the enabled feature set", () => {
  const running = ["resolve-conflict", "review-in-editor"];
  assert.deepEqual(settings.pendingRestart(running, { ...CURRENT, repos: { "ACME/other": "/x" } }), {
    required: false,
    added: [],
    removed: [],
  });
  assert.deepEqual(settings.pendingRestart(running, { ...CURRENT, enabledFeatures: ["resolve-conflict", "analyze-issue"] }), {
    required: true,
    added: ["analyze-issue"],
    removed: ["review-in-editor"],
  });
});

test("pendingRestart is unaffected by a sessionCache change — every reader is live", () => {
  const running = ["resolve-conflict", "review-in-editor"];
  assert.equal(
    settings.pendingRestart(running, { ...CURRENT, sessionCache: { heartbeat: true } }).required,
    false,
  );
});

test("suggestedRepos skips clones already mapped by key or folder", () => {
  const clones = [
    { key: "ACME/Sample-App", path: "/elsewhere/sample-app" },
    { key: "ACME/alias", path: "/Users/me/gitviews/sample-app/" },
    { key: "ACME/new", path: "/Users/me/gitviews/new" },
  ];
  assert.deepEqual(settings.suggestedRepos(CURRENT.repos, clones), [{ key: "ACME/new", path: "/Users/me/gitviews/new" }]);
});

test("writeConfigFile replaces the file atomically and keeps its permissions", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "settings-test-"));
  try {
    const file = path.join(dir, "config.json");
    fs.writeFileSync(file, "{}", { mode: 0o640 });
    fs.chmodSync(file, 0o640);
    settings.writeConfigFile(file, { a: 1 });
    assert.deepEqual(JSON.parse(fs.readFileSync(file, "utf8")), { a: 1 });
    assert.equal(fs.statSync(file).mode & 0o777, 0o640);
    assert.deepEqual(fs.readdirSync(dir), ["config.json"]);

    const fresh = path.join(dir, "new.json");
    settings.writeConfigFile(fresh, { b: 2 });
    assert.equal(fs.statSync(fresh).mode & 0o777, 0o600);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("validateSettingsUpdate accepts a real clone with a matching origin", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "settings-test-"));
  try {
    const clone = path.join(dir, "sample-app");
    fs.mkdirSync(clone);
    execFileSync("git", ["init", "-q"], { cwd: clone });
    execFileSync("git", ["remote", "add", "origin", "https://bitbucket.example.com/scm/acme/sample-app.git"], {
      cwd: clone,
    });
    assert.deepEqual(settings.validateSettingsUpdate({ repos: { "ACME/sample-app": clone } }, { repos: {} }), []);
    const wrong = settings.validateSettingsUpdate({ repos: { "ACME/api": clone } }, { repos: {} });
    assert.equal(wrong.length, 1);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// ---- migrateTokensToStore ----

test("migrateTokensToStore moves non-empty jira/jenkins/bitbucket apiToken into the store, blanks and saves config", () => {
  const store = fakeStore();
  const config = {
    jira: { baseUrl: "https://jira.example.com", apiToken: "jira-tok" },
    jenkins: { baseUrl: "https://jenkins.example.com", apiToken: "jenkins-tok" },
    bitbucket: { apiToken: "bb-tok" },
  };
  let saved = null;
  const result = settings.migrateTokensToStore(config, store, (c) => (saved = c));

  assert.deepEqual(result.migrated.sort(), ["bitbucket.apiToken", "jenkins.apiToken", "jira.apiToken"]);
  assert.equal(store.get("jira.apiToken"), "jira-tok");
  assert.equal(store.get("jenkins.apiToken"), "jenkins-tok");
  assert.equal(store.get("bitbucket.apiToken"), "bb-tok");
  assert.equal(config.jira.apiToken, undefined);
  assert.equal(config.jenkins.apiToken, undefined);
  assert.equal(config.bitbucket.apiToken, undefined);
  assert.equal(config.jira.baseUrl, "https://jira.example.com", "other fields untouched");
  assert.strictEqual(saved, config, "saveConfig is called with the (now blanked) config");
});

test("migrateTokensToStore ignores mcp/slack and any blank or missing apiToken", () => {
  const store = fakeStore();
  const config = {
    jira: { baseUrl: "https://jira.example.com", apiToken: "" },
    jenkins: { baseUrl: "https://jenkins.example.com" },
    mcp: { token: "should-not-be-touched" },
    slack: { token: "should-not-be-touched-either" },
  };
  let savedCalls = 0;
  const result = settings.migrateTokensToStore(config, store, () => savedCalls++);

  assert.deepEqual(result.migrated, []);
  assert.equal(savedCalls, 0);
  assert.deepEqual(store.list(), []);
  assert.equal(config.mcp.token, "should-not-be-touched");
  assert.equal(config.slack.token, "should-not-be-touched-either");
});

test("migrateTokensToStore is idempotent — a second run against the already-blanked config does nothing", () => {
  const store = fakeStore();
  const config = { jira: { baseUrl: "https://jira.example.com", apiToken: "jira-tok" } };
  let saveCalls = 0;
  settings.migrateTokensToStore(config, store, () => saveCalls++);
  assert.equal(saveCalls, 1);

  const second = settings.migrateTokensToStore(config, store, () => saveCalls++);
  assert.deepEqual(second.migrated, []);
  assert.equal(saveCalls, 1, "saveConfig was not called again");
  assert.equal(store.get("jira.apiToken"), "jira-tok", "still there from the first run");
});

test("migrateTokensToStore leaves config untouched and warns once on a store failure", () => {
  const config = {
    jira: { baseUrl: "https://jira.example.com", apiToken: "jira-tok" },
    jenkins: { apiToken: "jenkins-tok" },
  };
  const failingStore = {
    set: () => {
      throw new Error("disk full");
    },
  };
  let saveCalls = 0;
  const originalWarn = console.warn;
  const warnings = [];
  console.warn = (msg) => warnings.push(msg);
  try {
    const result = settings.migrateTokensToStore(config, failingStore, () => saveCalls++);
    assert.deepEqual(result.migrated, []);
    assert.equal(saveCalls, 0);
    assert.equal(config.jira.apiToken, "jira-tok", "config untouched on failure");
    assert.equal(config.jenkins.apiToken, "jenkins-tok");
    assert.equal(warnings.length, 1, "exactly one WARN, not one per token");
  } finally {
    console.warn = originalWarn;
  }
});

test("publicSettings reports the history retention, defaulting to 180 days", () => {
  assert.equal(settings.publicSettings(CURRENT, fakeCredentials([])).history.retentionDays, 180);
  const custom = { ...CURRENT, history: { retentionDays: 30 } };
  assert.equal(settings.publicSettings(custom, fakeCredentials([])).history.retentionDays, 30);
});

test("validateSettingsUpdate accepts a retention of 7 to 3650 whole days and rejects the rest", () => {
  const check = (history) => settings.validateSettingsUpdate({ history }, CURRENT, fakeDeps());
  assert.deepEqual(check({ retentionDays: 30 }), []);
  assert.equal(check({ retentionDays: 3 })[0].field, "history.retentionDays");
  assert.equal(check({ retentionDays: 30.5 })[0].field, "history.retentionDays");
  assert.equal(check({ retentionDays: "30" })[0].field, "history.retentionDays");
  assert.equal(check({ nope: 1 })[0].field, "history.nope");
});

test("mergeSettings stores a non-default retention and omits the default", () => {
  const set = settings.mergeSettings(CURRENT, { history: { retentionDays: 30 } }, {});
  assert.deepEqual(set.config.history, { retentionDays: 30 });
  const reset = settings.mergeSettings(set.config, { history: { retentionDays: 180 } }, {});
  assert.equal(reset.config.history, undefined);
});

test("publicSettings reports the shared test-history URL, blank by default", () => {
  assert.equal(settings.publicSettings(CURRENT, fakeCredentials([])).riskFacts.url, "");
  const set = { ...CURRENT, riskFacts: { url: "https://facts.example/risk.json" } };
  assert.equal(settings.publicSettings(set, fakeCredentials([])).riskFacts.url, "https://facts.example/risk.json");
});

test("validateSettingsUpdate accepts an http(s) URL or blank for riskFacts.url and rejects the rest", () => {
  const check = (riskFacts) => settings.validateSettingsUpdate({ riskFacts }, CURRENT, fakeDeps());
  assert.deepEqual(check({ url: "https://facts.example/risk.json" }), []);
  assert.deepEqual(check({ url: "" }), []);
  assert.equal(check({ url: "ftp://x/y" })[0].field, "riskFacts.url");
  assert.equal(check({ url: "not a url" })[0].field, "riskFacts.url");
  assert.equal(check({ url: "https://a/b\nc" })[0].field, "riskFacts.url");
  assert.equal(check({ url: `https://x/${"a".repeat(600)}` })[0].field, "riskFacts.url");
  assert.equal(check({ nope: 1 })[0].field, "riskFacts.nope");
});

test("mergeSettings stores a trimmed URL and removes riskFacts when blank", () => {
  const set = settings.mergeSettings(CURRENT, { riskFacts: { url: "  https://facts.example/risk.json  " } }, {}).config;
  assert.deepEqual(set.riskFacts, { url: "https://facts.example/risk.json" });
  const cleared = settings.mergeSettings(set, { riskFacts: { url: "" } }, {}).config;
  assert.equal(cleared.riskFacts, undefined);
});

const HELM = { helmJob: "helm_main", appJob: "web_main", browserJob: "browser_main" };




test("ticketToPr.reviewTransitionName: shown with its default, validated, and only saved when not the default", () => {
  assert.equal(settings.publicSettings(CURRENT, fakeCredentials([])).ticketToPr.reviewTransitionName, "In Review");
  const check = (ticketToPr) => settings.validateSettingsUpdate({ ticketToPr }, CURRENT, fakeDeps());
  assert.deepEqual(check({ reviewTransitionName: "Code Review" }), []);
  assert.deepEqual(check({ reviewTransitionName: "" }), []);
  assert.equal(check({ reviewTransitionName: "a\nb" })[0].field, "ticketToPr.reviewTransitionName");
  assert.equal(check({ reviewTransitionName: "x".repeat(101) })[0].field, "ticketToPr.reviewTransitionName");
  assert.equal(check({ reviewTransitionName: 5 })[0].field, "ticketToPr.reviewTransitionName");
  assert.equal(check({ other: 1 })[0].field, "ticketToPr.other");
  assert.equal(check("x")[0].field, "ticketToPr");
  const set = settings.mergeSettings(CURRENT, { ticketToPr: { reviewTransitionName: "  Code Review " } }, {}).config;
  assert.deepEqual(set.ticketToPr, { reviewTransitionName: "Code Review" });
  assert.equal(settings.publicSettings(set, fakeCredentials([])).ticketToPr.reviewTransitionName, "Code Review");
  assert.equal(settings.mergeSettings(set, { ticketToPr: { reviewTransitionName: "In Review" } }, {}).config.ticketToPr, undefined);
  assert.equal(settings.mergeSettings(set, { ticketToPr: { reviewTransitionName: "" } }, {}).config.ticketToPr, undefined);
});

test("ticketToPr.autoMoveToReview: on by default, validated, only saved when turned off, and kept beside the transition name", () => {
  assert.equal(settings.publicSettings(CURRENT, fakeCredentials([])).ticketToPr.autoMoveToReview, true);
  const check = (ticketToPr) => settings.validateSettingsUpdate({ ticketToPr }, CURRENT, fakeDeps());
  assert.deepEqual(check({ autoMoveToReview: false }), []);
  assert.equal(check({ autoMoveToReview: "no" })[0].field, "ticketToPr.autoMoveToReview");
  const off = settings.mergeSettings(CURRENT, { ticketToPr: { autoMoveToReview: false } }, {}).config;
  assert.deepEqual(off.ticketToPr, { autoMoveToReview: false });
  assert.equal(settings.publicSettings(off, fakeCredentials([])).ticketToPr.autoMoveToReview, false);
  // The panel sends both fields together: neither wipes the other.
  const both = settings.mergeSettings(off, { ticketToPr: { reviewTransitionName: "Code Review", autoMoveToReview: false } }, {}).config;
  assert.deepEqual(both.ticketToPr, { reviewTransitionName: "Code Review", autoMoveToReview: false });
  const nameOnly = settings.mergeSettings(both, { ticketToPr: { reviewTransitionName: "In Review" } }, {}).config;
  assert.deepEqual(nameOnly.ticketToPr, { autoMoveToReview: false });
  // Back to the defaults: nothing is saved.
  assert.equal(settings.mergeSettings(both, { ticketToPr: { reviewTransitionName: "", autoMoveToReview: true } }, {}).config.ticketToPr, undefined);
});

test("summarizeComments.model: blank by default, validated like other models, saved trimmed and cleared when blank", () => {
  assert.equal(settings.publicSettings(CURRENT, fakeCredentials([])).summarizeComments.model, "");
  const check = (summarizeComments) => settings.validateSettingsUpdate({ summarizeComments }, CURRENT, fakeDeps());
  assert.deepEqual(check({ model: "claude-haiku-4-5-20251001" }), []);
  assert.deepEqual(check({ model: "" }), []);
  assert.equal(check({ model: "rm -rf /" })[0].field, "summarizeComments.model");
  assert.equal(check({ other: 1 })[0].field, "summarizeComments.other");
  assert.equal(check("x")[0].field, "summarizeComments");
  const set = settings.mergeSettings(CURRENT, { summarizeComments: { model: " haiku " } }, {}).config;
  assert.deepEqual(set.summarizeComments, { model: "haiku" });
  assert.equal(settings.mergeSettings(set, { summarizeComments: { model: "" } }, {}).config.summarizeComments, undefined);
});

test("addressReviewComments.model: validated like other models, saved trimmed, and checkCommands kept", () => {
  assert.equal(settings.publicSettings(CURRENT, fakeCredentials([])).addressReviewComments.model, "");
  const check = (addressReviewComments) => settings.validateSettingsUpdate({ addressReviewComments }, CURRENT, fakeDeps());
  assert.deepEqual(check({ model: "claude-opus-4-8[context=1m,effort=high]" }), []);
  assert.equal(check({ model: "rm -rf /" })[0].field, "addressReviewComments.model");
  assert.equal(check({ checkCommands: ["npm test"] })[0].field, "addressReviewComments.checkCommands");
  const withChecks = { ...CURRENT, addressReviewComments: { checkCommands: ["npm test"] } };
  const set = settings.mergeSettings(withChecks, { addressReviewComments: { model: " gpt-5 " } }, {}).config;
  assert.deepEqual(set.addressReviewComments, { checkCommands: ["npm test"], model: "gpt-5" });
  const cleared = settings.mergeSettings(set, { addressReviewComments: { model: "" } }, {}).config;
  assert.deepEqual(cleared.addressReviewComments, { checkCommands: ["npm test"] });
});

test("reviewEditor: VS Code is hidden — rejected on save and shown as automatic if already saved", () => {
  const errors = settings.validateSettingsUpdate({ reviewEditor: "vscode" }, CURRENT, fakeDeps());
  assert.equal(errors[0].field, "reviewEditor");
  assert.equal(settings.publicSettings({ ...CURRENT, reviewEditor: "vscode" }, fakeCredentials([])).reviewEditor, null);
});
