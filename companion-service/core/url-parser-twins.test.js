// chrome-extension/jenkins-urls.js (jenkinsBuildFromUrl) and
// core/diagnose-prompt.js (parseBuildUrl) are two copies of one parser; this
// runs one table of URLs through both. They intentionally differ on the
// origin check (the companion also requires the configured Jenkins origin),
// so only accept/reject and the path part of the rebuilt URL are compared.
const test = require("node:test");
const assert = require("node:assert/strict");
const { jenkinsBuildFromUrl } = require("../../chrome-extension/jenkins-urls.js");
const { parseBuildUrl } = require("./diagnose-prompt.js");

const BASE = "https://jenkins.example.com";

const ACCEPT = [
  `${BASE}/job/web_main/5940/`,
  `${BASE}/job/web_main/5940/console`,
  `${BASE}/job/team/job/repo%20one/job/main/12/console`,
  `${BASE}/blue/organizations/jenkins/helm_main/detail/helm_main/812/pipeline`,
  `${BASE}/blue/organizations/jenkins/team%2Frepo/detail/main/5/pipeline`,
  `${BASE}/blue/organizations/jenkins/team%2Frepo/detail/repo/5/pipeline`,
];
const REJECT = [
  `${BASE}/job/web_main/0/`,
  `${BASE}/job/web_main/`,
  `${BASE}/job/../5/`,
  `${BASE}/job/a%2Fb/5/`,
  `${BASE}/job/bad$name/5/`,
  `${BASE}/job/%E0%A4%A/5/`,
  `${BASE}/job/x/1234567890/`,
  `${BASE}/blue/organizations/jenkins/feature%2Fx/detail/feature%2Fx/1/pipeline`,
  `${BASE}/blue/organizations/jenkins/p/detail/p/`,
  `${BASE}/`,
  "not a url",
];

const pathOf = (u) => new URL(u).pathname;

test("both parsers accept the same URLs and rebuild the same job path", () => {
  for (const u of ACCEPT) {
    const a = jenkinsBuildFromUrl(u);
    const b = parseBuildUrl(u, BASE);
    assert.ok(a, `extension accepts ${u}`);
    assert.ok(b, `companion accepts ${u}`);
    assert.equal(pathOf(a.buildUrl), pathOf(b.buildUrl), u);
    assert.equal(a.jobName, b.jobName, u);
    assert.equal(a.number, b.number, u);
  }
});

test("both parsers reject the same URLs", () => {
  for (const u of REJECT) {
    assert.equal(jenkinsBuildFromUrl(u), null, `extension rejects ${u}`);
    assert.equal(parseBuildUrl(u, BASE), null, `companion rejects ${u}`);
  }
});
