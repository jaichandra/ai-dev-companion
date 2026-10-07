const test = require("node:test");
const assert = require("node:assert/strict");
const { chooseTier, MAX_ONPREM_BYTES } = require("./route-policy.js");
const { CLAUDE_POLICIES, isSandboxedPolicy } = require("./claude-args.js");

const ON = { onpremAvailable: true };

test("code changes, tool use, diagnosis and analysis always go to Claude", () => {
  for (const task of ["code-change", "diagnosis", "analysis"]) assert.equal(chooseTier({ task }, ON).tier, "claude", task);
  assert.equal(chooseTier({ task: "triage", needsTools: true }, ON).tier, "claude");
  assert.equal(chooseTier({ task: "something-new" }, ON).tier, "claude");
});

test("the small text-only steps go on-prem only when the proxy is available and the text fits", () => {
  for (const task of ["triage", "classify", "condense", "embed"]) {
    assert.equal(chooseTier({ task, bytes: 1000 }, ON).tier, "onprem", task);
    assert.equal(chooseTier({ task, bytes: 1000 }, { onpremAvailable: false }).tier, "claude", task);
  }
  assert.equal(chooseTier({ task: "triage", bytes: MAX_ONPREM_BYTES + 1 }, ON).tier, "claude");
  assert.equal(chooseTier({ task: "triage" }).tier, "claude", "no proxy info means Claude");
  assert.match(chooseTier({ task: "triage" }, { onpremAvailable: false }).reason, /isn't available/);
});

test("readOnlyBackground is readOnly without WebFetch and WebSearch", () => {
  const bg = CLAUDE_POLICIES.readOnlyBackground;
  assert.deepEqual(bg.tools, CLAUDE_POLICIES.readOnly.tools.filter((t) => t !== "WebFetch" && t !== "WebSearch"));
  assert.equal(bg.mcpGuard, true);
  assert.equal(bg.denyCwdWrite, true);
  assert.equal(isSandboxedPolicy(bg), true);
  assert.ok(!bg.tools.some((t) => /^Web/.test(t)));
});

test("the byte limit is inclusive; NaN, Infinity, negative and non-numeric sizes are handled without throwing", () => {
  assert.equal(chooseTier({ task: "triage", bytes: MAX_ONPREM_BYTES }, ON).tier, "onprem", "exactly the limit still fits");
  assert.equal(chooseTier({ task: "triage", bytes: MAX_ONPREM_BYTES + 1 }, ON).tier, "claude");
  for (const bytes of [NaN, Infinity, -Infinity]) {
    const r = chooseTier({ task: "triage", bytes }, ON);
    assert.equal(r.tier, "claude", String(bytes));
    assert.match(r.reason, /too large/);
  }
  assert.equal(chooseTier({ task: "triage", bytes: 0 }, ON).tier, "onprem");
  assert.equal(chooseTier({ task: "triage", bytes: -5 }, ON).tier, "onprem", "a negative size is just small");
  assert.doesNotThrow(() => chooseTier({ task: "triage", bytes: "big" }, ON));
  assert.equal(chooseTier({ task: "triage", bytes: "big" }, ON).tier, "claude");
  assert.doesNotThrow(() => chooseTier(undefined, undefined));
  assert.equal(chooseTier({}, ON).tier, "claude", "no task, no on-prem rule");
  assert.equal(chooseTier({ task: "x".repeat(500) }, ON).reason.length < 80, true, "an odd task name is clipped in the reason");
});
