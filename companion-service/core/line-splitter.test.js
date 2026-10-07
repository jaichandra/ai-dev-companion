const test = require("node:test");
const assert = require("node:assert/strict");
const { createLineSplitter } = require("./line-splitter.js");

test("splits chunks on newlines and joins a line split across chunks", () => {
  const lines = [];
  const s = createLineSplitter((l) => lines.push(l));
  s.write('{"a":1}\n{"b"');
  s.write(':2}\n\r\n{"c":3}');
  assert.deepEqual(lines, ['{"a":1}', '{"b":2}']);
  s.flush();
  assert.deepEqual(lines, ['{"a":1}', '{"b":2}', '{"c":3}']);
});

test("drops carriage returns, ignores blanks, and survives a throwing listener", () => {
  const lines = [];
  const s = createLineSplitter((l) => {
    lines.push(l);
    throw new Error("boom");
  });
  s.write("one\r\n\ntwo\n");
  s.flush();
  assert.deepEqual(lines, ["one", "two"]);
});
