const test = require("node:test");
const assert = require("node:assert/strict");
const { linkOnHosts } = require("./link-guard.js");

const BASES = ["https://bb.example", "https://jira.example:8443/"];

test("only https links on a configured host survive", () => {
  assert.equal(linkOnHosts("https://bb.example/projects/CI", BASES), "https://bb.example/projects/CI");
  assert.equal(linkOnHosts("https://BB.example/x", BASES), "https://bb.example/x");
  assert.equal(linkOnHosts("https://jira.example:8443/browse/PROJ-1", BASES), "https://jira.example:8443/browse/PROJ-1");
  for (const bad of [
    "http://bb.example/x",
    "https://evil.example/x",
    "https://jira.example/x", // wrong port
    "https://bb.example.evil.example/x",
    "https://user:pw@bb.example/x",
    "javascript:alert(1)",
    "//bb.example/x",
    "not a url",
    "",
    null,
    undefined,
    42,
  ]) {
    assert.equal(linkOnHosts(bad, BASES), null, String(bad));
  }
  assert.equal(linkOnHosts("https://bb.example/x", []), null, "no configured host allows nothing");
  assert.equal(linkOnHosts("https://bb.example/x", ["nonsense"]), null);
});
