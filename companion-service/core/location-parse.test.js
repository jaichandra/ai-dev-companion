const test = require("node:test");
const assert = require("node:assert/strict");
const { parseLocations } = require("./location-parse.js");

const at = (text) => parseLocations(text).map((l) => `${l.path}:${l.line}:${l.column}`);

test("Node/Chrome stack frames: absolute path with line and column", () => {
  assert.deepEqual(
    at("    at Object.<anonymous> (/home/ci/workspace/sample-app/src/app/login.ts:42:13)\n    at run (/home/ci/workspace/sample-app/src/run.js:7:1)"),
    ["/home/ci/workspace/sample-app/src/app/login.ts:42:13", "/home/ci/workspace/sample-app/src/run.js:7:1"],
  );
  assert.deepEqual(at("    at /work/tests/login.spec.ts:10:5"), ["/work/tests/login.spec.ts:10:5"]);
});

test("webpack and file:// prefixes and ?query suffixes are stripped", () => {
  assert.deepEqual(at("at Module.foo (webpack:///./src/components/Button.tsx?2f3a:15:9)"), ["src/components/Button.tsx:15:9"]);
  assert.deepEqual(at("at x (webpack://app/./src/x.js:3:4)"), ["src/x.js:3:4"]);
  assert.deepEqual(at("at y (webpack-internal:///./src/y.jsx:8:2)"), ["src/y.jsx:8:2"]);
  assert.deepEqual(at("at z (file:///Users/me/proj/x.js:10:2)"), ["/Users/me/proj/x.js:10:2"]);
});

test("TypeScript compiler style path(line,col)", () => {
  assert.deepEqual(at("src/index.ts(12,34): error TS2322: Type 'x' is not assignable"), ["src/index.ts:12:34"]);
});

test("Python tracebacks", () => {
  assert.deepEqual(
    at('Traceback (most recent call last):\n  File "/app/tests/test_login.py", line 57, in test_x\n    assert ok'),
    ["/app/tests/test_login.py:57:null"],
  );
});

test("Java frames become package paths, and are not read twice", () => {
  assert.deepEqual(
    at("\tat com.acme.ci.LoginPage.click(LoginPage.java:88)\n\tat org.junit.Assert.fail(Assert.java:89)"),
    ["com/acme/ci/LoginPage.java:88:null", "org/junit/Assert.java:89:null"],
  );
});

test("Go panics", () => {
  assert.deepEqual(at("main.handler(...)\n\t/go/src/app/handler.go:57 +0x1d"), ["/go/src/app/handler.go:57:null"]);
});

test("a relative path with no column", () => {
  assert.deepEqual(at("Error at tests/login.spec.ts:10"), ["tests/login.spec.ts:10:null"]);
});

test("duplicates are dropped and the order of first appearance is kept", () => {
  assert.deepEqual(at("a.ts:1:1 b.ts:2:2 a.ts:1:1"), ["a.ts:1:1", "b.ts:2:2"]);
});

test("non-source files, URLs, path traversal and junk yield nothing", () => {
  assert.deepEqual(at("screenshot.png:3"), []);
  assert.deepEqual(at("at https://cdn.example.com/bundle.js:1:2345"), []);
  assert.deepEqual(at("(../../etc/secrets.py:1)"), []);
  assert.deepEqual(at("nothing to see here"), []);
  assert.deepEqual(parseLocations(""), []);
  assert.deepEqual(parseLocations(undefined), []);
  assert.deepEqual(parseLocations(42), []);
});

test("absurd positions are ignored, and the text and result count are capped", () => {
  assert.deepEqual(at("a.ts:0:1 b.ts:99999999999:1"), []);
  const many = Array.from({ length: 50 }, (_, i) => `f${i}.ts:${i + 1}:1`).join(" ");
  assert.equal(parseLocations(many).length, 20);
  const huge = `${"x".repeat(9000)} tail.ts:5:5`;
  assert.deepEqual(parseLocations(huge), []); // beyond the 8 KB cap
});
