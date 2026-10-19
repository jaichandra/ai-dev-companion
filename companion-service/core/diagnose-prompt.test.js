const test = require("node:test");
const assert = require("node:assert/strict");
const diagnosePrompt = require("./diagnose-prompt.js");
const { parseBuildUrl, flakyContext, buildBasicPrompt } = diagnosePrompt;

const BASE = "https://jenkins.example.com";

test("parseBuildUrl: classic job and folder-job build URLs", () => {
  assert.deepEqual(parseBuildUrl(`${BASE}/job/web_main/5940/`, BASE), {
    jobName: "web_main",
    number: 5940,
    buildUrl: `${BASE}/job/web_main/5940/`,
  });
  assert.deepEqual(parseBuildUrl(`${BASE}/job/team/job/repo%20one/job/main/12/console`, BASE), {
    jobName: "team/repo one/main",
    number: 12,
    buildUrl: `${BASE}/job/team/job/repo%20one/job/main/12/`,
  });
});

test("parseBuildUrl: Blue Ocean single and multibranch pipelines", () => {
  assert.equal(
    parseBuildUrl(`${BASE}/blue/organizations/jenkins/helm_main/detail/helm_main/812/pipeline`, BASE).buildUrl,
    `${BASE}/job/helm_main/812/`,
  );
  assert.deepEqual(parseBuildUrl(`${BASE}/blue/organizations/jenkins/team%2Frepo/detail/main/5/pipeline`, BASE), {
    jobName: "team/repo/main",
    number: 5,
    buildUrl: `${BASE}/job/team/job/repo/job/main/5/`,
  });
});

test("parseBuildUrl refuses other origins, credentials, junk names and non-build pages", () => {
  for (const bad of [
    "https://evil.example.com/job/web_main/5940/",
    `${BASE.replace("https://", "https://user:pw@")}/job/web_main/5940/`,
    "http://jenkins.example.com/job/web_main/5940/",
    `${BASE}/job/web_main/`,
    `${BASE}/job/web_main/0/`,
    `${BASE}/job/web_main/12x/`,
    `${BASE}/job/../5/`,
    `${BASE}/job/a%2Fb/5/`,
    `${BASE}/job/bad$name/5/`,
    `${BASE}/job/%E0%A4%A/5/`,
    `${BASE}/blue/organizations/jenkins/feature%2Fx/detail/feature%2Fx/1/pipeline`,
    `${BASE}/`,
    "not a url",
    "javascript:alert(1)",
  ]) {
    assert.equal(parseBuildUrl(bad, BASE), null, bad);
  }
  assert.equal(parseBuildUrl(`${BASE}/job/web_main/5940/`, "not a url"), null);
});

const facts = {
  tests: new Map([
    ["login with sso", { name: "Login with SSO", flaky: true, failed: 8, of: 30 }],
    ["export report", { name: "Export report", flaky: false, failed: 1, of: 30 }],
  ]),
};

test("flakyContext lists only the failing tests the feed calls flaky, once each", () => {
  assert.deepEqual(flakyContext(facts, ["Login with SSO", "export report", "Unknown", "  login with sso  "]), [
    '- "Login with SSO" is known to be flaky (failed 8 of the last 30 builds).',
  ]);
  assert.deepEqual(flakyContext(facts, []), []);
  assert.deepEqual(flakyContext(undefined, ["x"]), []);
});

test("flakyContext makes a hostile name single-line and coerces odd counts", () => {
  const f = {
    tests: new Map([
      ["a", { name: "evil\nname\u001b[31m  red\t\"x\"", flaky: true, failed: 2.5, of: "30" }],
      ["b", { name: "ok", flaky: true, failed: -1, of: Infinity }],
    ]),
  };
  const [a, b] = flakyContext(f, ["a", "b"]);
  assert.equal(
    a,
    '- "evil name [31m red "x"" is known to be flaky (failed 0 of the last 0 builds).');
  assert.doesNotMatch(a, /[\u0000-\u001f]/);
  assert.equal(b, '- "ok" is known to be flaky (failed 0 of the last 0 builds).');
});

test("flakyContext caps at five lines", () => {
  const many = { tests: new Map(Array.from({ length: 9 }, (_, i) => [`t${i}`, { name: `T${i}`, flaky: true, failed: 1, of: 2 }])) };
  assert.equal(flakyContext(many, Array.from({ length: 9 }, (_, i) => `t${i}`)).length, 5);
});

test("buildBasicPrompt names the build, asks for a read-only diagnosis, and adds flaky context only when there is some", () => {
  const plain = buildBasicPrompt({ buildUrl: `${BASE}/job/x/7/`, jobName: "x", number: 7 });
  assert.match(plain, /Diagnose why Jenkins build #7 of "x" failed\./);
  assert.match(plain, /Build: https:\/\/jenkins\.example\.com\/job\/x\/7\//);
  assert.match(plain, /Do not change any files/);
  assert.ok(!plain.includes("shared test history"));
  const withFlaky = buildBasicPrompt({ buildUrl: `${BASE}/job/x/7/`, jobName: "x", number: 7, flakyLines: ['- "A" is known to be flaky (failed 1 of the last 2 builds).'] });
  assert.match(withFlaky, /shared test history \(it may be out of date\):\n- "A" is known to be flaky/);
});

test("buildReportPrompt asks for read-only work and a JSON-only answer, with flaky context when there is some", () => {
  const prompt = diagnosePrompt.buildReportPrompt({ buildUrl: `${BASE}/job/x/7/`, jobName: "x", number: 7, flakyLines: ["- flaky"] });
  assert.match(prompt, /Diagnose why Jenkins build #7 of "x" failed\./);
  assert.match(prompt, /Do not change any files/);
  assert.match(prompt, /ONLY a JSON object/);
  assert.match(prompt, /"stages"/);
  assert.match(prompt, /shared test history[^\n]*\n- flaky/);
  assert.ok(!diagnosePrompt.buildReportPrompt({ buildUrl: `${BASE}/job/x/7/`, jobName: "x", number: 7 }).includes("shared test history"));
});

test("buildStructurePrompt wraps the diagnosis as data and clips it", () => {
  const prompt = diagnosePrompt.buildStructurePrompt("a".repeat(50000));
  assert.match(prompt, /<diagnosis>\na+\n<\/diagnosis>/);
  assert.ok(prompt.length < 42000);
});

test("parseReport reads the JSON, tolerating prose and a code fence, and normalises it", () => {
  const out = diagnosePrompt.parseReport(
    'Here you go:\n```json\n' +
      JSON.stringify({
        tldr: " Timeout in a widget spec. ",
        category: "Regression",
        causes: [{ title: "Timeout", where: "a.spec.ts:84", detail: "waits 5s" }, { title: "" }, null],
        suspects: [{ title: "a3f9c21", detail: "Dana" }],
        failingTests: [{ name: "Dashboard load", flaky: true, note: "3 of 20" }, { name: "x", flaky: "yes" }],
        stages: [{ name: "Unit tests", result: "PASSED", duration: "6m" }, { name: "Odd", result: "weird", duration: "unknown" }],
        nextSteps: ["Re-run", "", 3],
      }) +
      "\n```",
  );
  assert.equal(out.tldr, "Timeout in a widget spec.");
  assert.equal(out.category, "regression");
  assert.deepEqual(out.causes, [{ title: "Timeout", where: "a.spec.ts:84", detail: "waits 5s" }]);
  assert.deepEqual(out.failingTests.map((t) => t.flaky), [true, false]);
  assert.deepEqual(out.stages.map((s) => s.result), ["passed", "unknown"]);
  assert.deepEqual(out.stages.map((s) => s.duration), ["6m", ""]);
  assert.deepEqual(out.nextSteps, ["Re-run"]);
});

test("parseReport falls back to the raw text, and throws on nothing", () => {
  assert.deepEqual(diagnosePrompt.parseReport("It timed out."), { raw: "It timed out." });
  assert.deepEqual(diagnosePrompt.parseReport('{"category":"flaky"}'), { raw: '{"category":"flaky"}' });
  assert.equal(diagnosePrompt.parseReport('{"tldr":"x","category":"cosmic"}').category, "unknown");
  assert.throws(() => diagnosePrompt.parseReport("  "), /empty/);
});
