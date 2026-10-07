const test = require("node:test");
const assert = require("node:assert/strict");
const environment = require("./environment.js");

test("every site has a unique id, a known kind and an https base URL", () => {
  const ids = environment.siteIds();
  assert.equal(new Set(ids).size, ids.length);
  for (const site of environment.sites) {
    assert.ok(["git", "issues", "ci"].includes(site.kind), site.id);
    assert.match(site.baseUrl, /^https:\/\/[^/]+$/, site.id);
  }
});

test("defaultBaseUrl looks a site up by id and is empty for an unknown one", () => {
  assert.equal(environment.defaultBaseUrl("jira"), environment.siteById("jira").baseUrl);
  assert.equal(environment.defaultBaseUrl("nope"), "");
});

test("siteCredentialNames follows the site ids", () => {
  assert.deepEqual(
    environment.siteCredentialNames(),
    environment.siteIds().map((id) => `${id}.apiToken`),
  );
});

test("the default LLM host matches its own allowed suffixes", () => {
  const host = new URL(environment.llm.baseUrl).hostname;
  assert.ok(environment.llm.hostSuffixes.some((s) => host.endsWith(s.replace(/^\./, ""))));
});

test("consumers see the profile's values", () => {
  assert.deepEqual(require("./core/updater.js").DEFAULT_SOURCE, environment.updateSource);
  assert.deepEqual(require("./core/targets.js").DEFAULT_PROJECTS, environment.targets.projects);
  assert.equal(require("./core/llm-proxy-request.js").DEFAULTS.baseUrl, environment.llm.baseUrl);
});
