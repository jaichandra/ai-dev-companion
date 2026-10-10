const test = require("node:test");
const assert = require("node:assert/strict");
const { isPlaceholderUrl, sitesNeedingUrl, normalizeUrl, promptSiteUrls } = require("./site-urls.js");

const env = {
  sites: [
    { id: "jira", label: "Jira", baseUrl: "https://jira.example.com" },
    { id: "jenkins", label: "Jenkins", baseUrl: "https://jenkins.example.com" },
    { id: "github", label: "GitHub", baseUrl: "https://github.com" },
  ],
};

test("only example.com hosts count as placeholders", () => {
  assert.equal(isPlaceholderUrl("https://jira.example.com"), true);
  assert.equal(isPlaceholderUrl("https://example.com"), true);
  assert.equal(isPlaceholderUrl("https://github.com"), false);
  assert.equal(isPlaceholderUrl("https://notexample.com"), false);
  assert.equal(isPlaceholderUrl("garbage"), false);
});

test("a site already configured, or with a real default, is not asked", () => {
  assert.deepEqual(sitesNeedingUrl(env, {}).map((s) => s.id), ["jira", "jenkins"]);
  assert.deepEqual(sitesNeedingUrl(env, { jira: { baseUrl: "https://j.acme.io" } }).map((s) => s.id), ["jenkins"]);
});

test("normalizeUrl adds a scheme, drops trailing slashes and rejects non-http", () => {
  assert.equal(normalizeUrl("jira.acme.io/"), "https://jira.acme.io");
  assert.equal(normalizeUrl("http://ci.acme.io:8080//"), "http://ci.acme.io:8080");
  assert.equal(normalizeUrl("ftp://x"), "");
  assert.equal(normalizeUrl(""), "");
});

test("promptSiteUrls keeps existing site fields, re-asks on a bad answer and skips on Enter", async () => {
  const answers = ["not a url!!", "jira.acme.io", ""];
  const lines = [];
  const out = await promptSiteUrls(env, { jira: { username: "me" } }, { ask: async () => answers.shift(), log: (l) => lines.push(l) });
  assert.deepEqual(out, { jira: { username: "me", baseUrl: "https://jira.acme.io" } });
  assert.equal(answers.length, 0);
});

test("promptSiteUrls asks nothing for a profile with real hosts", async () => {
  const real = { sites: [{ id: "github", label: "GitHub", baseUrl: "https://github.com" }] };
  assert.deepEqual(await promptSiteUrls(real, {}, { ask: () => assert.fail("asked"), log() {} }), {});
});
