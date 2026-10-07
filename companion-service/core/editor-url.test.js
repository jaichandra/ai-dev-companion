const test = require("node:test");
const assert = require("node:assert/strict");
const { pickEditor, editorUrl } = require("./editor-url.js");

test("pickEditor prefers the configured editor when it can open a file at a line and is installed", () => {
  assert.equal(pickEditor({ configured: "vscode", installed: ["claude-code", "cursor", "vscode"] }), "vscode");
  assert.equal(pickEditor({ configured: "cursor", installed: ["cursor"] }), "cursor");
});

test("pickEditor falls back to Cursor, then VS Code, and never picks claude-code", () => {
  assert.equal(pickEditor({ configured: "claude-code", installed: ["claude-code", "vscode", "cursor"] }), "cursor");
  assert.equal(pickEditor({ configured: undefined, installed: ["vscode"] }), "vscode");
  assert.equal(pickEditor({ configured: "cursor", installed: ["vscode"] }), "vscode");
  assert.equal(pickEditor({ configured: "vscode", installed: ["claude-code"] }), null);
  assert.equal(pickEditor({ configured: "__proto__", installed: [] }), null);
});

test("editorUrl builds the scheme URL with an encoded path, line and column", () => {
  assert.equal(editorUrl("cursor", "/Users/me/proj/src/a.ts", 10, 5), "cursor://file/Users/me/proj/src/a.ts:10:5");
  assert.equal(editorUrl("vscode", "/Users/me/my proj/a#b.ts", 3, null), "vscode://file/Users/me/my%20proj/a%23b.ts:3:1");
});

test("editorUrl defaults and clamps the position, and refuses bad input", () => {
  assert.equal(editorUrl("cursor", "/a/b.ts", 0, -4), "cursor://file/a/b.ts:1:1");
  assert.equal(editorUrl("cursor", "/a/b.ts", "12", "3"), "cursor://file/a/b.ts:12:3");
  assert.throws(() => editorUrl("claude-code", "/a/b.ts", 1, 1), /Can't open a file at a line/);
  assert.throws(() => editorUrl("cursor", "relative/b.ts", 1, 1), /absolute path/);
  assert.throws(() => editorUrl("cursor", "/a/b\n.ts", 1, 1), /absolute path/);
});
