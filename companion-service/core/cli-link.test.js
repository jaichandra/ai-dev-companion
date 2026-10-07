const test = require("node:test");
const assert = require("node:assert/strict");
const { planLink } = require("./cli-link.js");

const base = { platform: "darwin", home: "/Users/me", stableDir: "/Users/me/ai-dev-companion" };
const TARGET = "/Users/me/ai-dev-companion/companion-service/bin/companion.js";

test("creates the link when nothing is there", () => {
  assert.deepEqual(planLink({ ...base, existing: { kind: "none" } }), {
    action: "create",
    linkPath: "/Users/me/.local/bin/companion",
    target: TARGET,
  });
});

test("leaves an existing correct link alone, and replaces one from an earlier install of this tool", () => {
  assert.equal(planLink({ ...base, existing: { kind: "symlink", target: TARGET } }).action, "leave");
  assert.equal(
    planLink({ ...base, existing: { kind: "symlink", target: "/old/place/companion-service/bin/companion.js" } }).action,
    "replace",
  );
});

test("never overwrites something else that is called companion", () => {
  const a = planLink({ ...base, existing: { kind: "other" } });
  assert.equal(a.action, "leave");
  assert.equal(a.reason, "something-else-is-there");
  assert.equal(planLink({ ...base, existing: { kind: "symlink", target: "/usr/bin/other-tool" } }).action, "leave");
});

test("skips Windows, where a symlink isn't the way", () => {
  assert.equal(planLink({ ...base, platform: "win32", existing: { kind: "none" } }).action, "skip");
});
