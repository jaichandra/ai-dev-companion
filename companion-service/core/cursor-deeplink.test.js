const test = require("node:test");
const assert = require("node:assert/strict");
const { buildCursorPromptUrl } = require("./cursor-deeplink.js");

test("buildCursorPromptUrl targets the worktree's window in Ask mode", () => {
  const url = new URL(buildCursorPromptUrl("/Users/me/wt/pr 42", "Review & don't edit"));
  assert.equal(url.protocol, "cursor:");
  assert.equal(url.host, "anysphere.cursor-deeplink");
  assert.equal(url.pathname, "/prompt");
  assert.equal(url.searchParams.get("text"), "Review & don't edit");
  assert.equal(url.searchParams.get("mode"), "ask");
  assert.equal(url.searchParams.get("workspace"), "pr 42");
});

test("buildCursorPromptUrl refuses prompts Cursor would reject", () => {
  assert.throws(() => buildCursorPromptUrl("/wt", "x".repeat(10000)), /too long/);
});
