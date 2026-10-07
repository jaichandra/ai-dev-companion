const test = require("node:test");
const assert = require("node:assert/strict");
const P = require("../../chrome-extension/jenkins-urls.js");

const BASE = "https://jenkins.example.com";

test("jenkinsBuildFromUrl reads classic and Blue Ocean build pages", () => {
  const classic = P.jenkinsBuildFromUrl(`${BASE}/job/team/job/repo%20one/job/main/12/console`);
  assert.deepEqual(classic, {
    jobName: "team/repo one/main",
    number: 12,
    buildUrl: `${BASE}/job/team/job/repo%20one/job/main/12/`,
    statusUrl: `${BASE}/job/team/job/repo%20one/job/main/12/api/json?tree=result,building`,
  });
  const blue = P.jenkinsBuildFromUrl(`${BASE}/blue/organizations/jenkins/helm_main/detail/helm_main/812/pipeline`);
  assert.equal(blue.buildUrl, `${BASE}/job/helm_main/812/`);
  assert.equal(blue.number, 812);
});

test("jenkinsBuildFromUrl refuses pages that are not a build", () => {
  for (const bad of [`${BASE}/job/web_main/`, `${BASE}/job/web_main/0/`, `${BASE}/job/a%2Fb/5/`, `${BASE}/`, "not a url", `${BASE}/job/bad$name/5/`]) {
    assert.equal(P.jenkinsBuildFromUrl(bad), null, bad);
  }
});

test("JENKINS_BUILD_PAGE matches build pages and not job or dashboard pages", () => {
  assert.ok(P.JENKINS_BUILD_PAGE.test(`${BASE}/job/web_main/5940/`));
  assert.ok(P.JENKINS_BUILD_PAGE.test(`${BASE}/job/a/job/b/12/console`));
  assert.ok(P.JENKINS_BUILD_PAGE.test(`${BASE}/blue/organizations/jenkins/x/detail/x/3/pipeline`));
  assert.ok(!P.JENKINS_BUILD_PAGE.test(`${BASE}/job/web_main/`));
  assert.ok(!P.JENKINS_BUILD_PAGE.test(`${BASE}/view/all/`));
});

