const test = require("node:test");
const assert = require("node:assert/strict");
const { parseArgs } = require("./cli-args.js");

test("no arguments means help", () => {
  assert.equal(parseArgs([]).command, "help");
});

test("commands, positionals and flags", () => {
  const p = parseArgs(["history", "PROJ-12", "--json", "--days", "7"]);
  assert.equal(p.command, "history");
  assert.deepEqual(p.positional, ["PROJ-12"]);
  assert.deepEqual(p.flags, { json: true, days: "7" });
  assert.deepEqual(p.errors, []);
  assert.equal(parseArgs(["forget", "PROJ-1", "-y"]).flags.yes, true);
  assert.equal(parseArgs(["open", "src/a.ts:10"]).command, "open");
  assert.deepEqual(parseArgs(["open", "src/a.ts:10", "extra"]).positional, ["src/a.ts:10", "extra"]);
  assert.equal(parseArgs(["jobs", "--feature", "analyze-issue"]).flags.feature, "analyze-issue");
});

test("unknown commands, unknown options and missing values are errors", () => {
  assert.match(parseArgs(["frobnicate"]).errors[0], /Unknown command/);
  assert.match(parseArgs(["jobs", "--wat"]).errors[0], /Unknown option/);
  assert.match(parseArgs(["jobs", "--feature"]).errors[0], /needs a value/);
  assert.match(parseArgs(["jobs", "--feature", "--json"]).errors[0], /needs a value/);
});

test("--help anywhere selects help", () => {
  assert.equal(parseArgs(["status", "--help"]).command, "help");
});
