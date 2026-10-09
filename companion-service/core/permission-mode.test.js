const test = require("node:test");
const assert = require("node:assert/strict");
const { permissionModeFor, DEFAULT_PERMISSION_MODE } = require("./permission-mode.js");
const { validateClaudeSection } = require("./environment-base.js");

test("defaults to auto with the placeholder profile", () => {
  assert.equal(DEFAULT_PERMISSION_MODE, "auto");
  assert.equal(permissionModeFor("review-in-editor"), "auto");
});

test("validateClaudeSection accepts nothing, valid modes, and rejects others", () => {
  validateClaudeSection(undefined);
  validateClaudeSection({ permissionMode: "plan", permissionModes: { "ticket-to-pr": "default" } });
  assert.throws(() => validateClaudeSection({ permissionMode: "bypassPermissions" }), /permissionMode/);
  assert.throws(() => validateClaudeSection({ permissionModes: { x: "nope" } }), /permissionModes\.x/);
  assert.throws(() => validateClaudeSection("auto"), /object/);
});
