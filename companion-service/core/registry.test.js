// chrome-extension/registry.js: the extension-side feature registry.
const test = require("node:test");
const assert = require("node:assert/strict");

function freshRegistry() {
  const path = require.resolve("../../chrome-extension/registry.js");
  delete require.cache[path];
  return require(path);
}

test("register adds entries and all() hands back them in registration order", () => {
  const registry = freshRegistry();
  registry.register({ id: "one", site: "git" });
  registry.register({ id: "two" });
  assert.deepEqual(registry.all().map((e) => e.id), ["one", "two"]);
});

test("all() is a copy: changing it doesn't change the registry", () => {
  const registry = freshRegistry();
  registry.register({ id: "one" });
  registry.all().push({ id: "sneaky" });
  assert.equal(registry.all().length, 1);
});

test("an entry needs an id, and an id can only be registered once", () => {
  const registry = freshRegistry();
  assert.throws(() => registry.register({}), /needs an id/);
  assert.throws(() => registry.register(null), /needs an id/);
  registry.register({ id: "one" });
  assert.throws(() => registry.register({ id: "one" }), /"one" is already registered/);
});

test("site must be one of the three kinds of server", () => {
  const registry = freshRegistry();
  for (const site of registry.SITES) registry.register({ id: `on-${site}`, site });
  assert.throws(() => registry.register({ id: "bad", site: "wiki" }), /has site "wiki", expected one of git, issues, ci/);
});
