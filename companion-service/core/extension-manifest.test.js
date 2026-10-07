// core/extension-manifest.js: the generated manifest, and the extension files it lists, really
// loaded together the way Chrome loads them (in order, one shared global scope).
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const manifestTool = require("./extension-manifest.js");
const packs = require("./packs.js");
const environment = require("../environment.js");

const EXTENSION_DIR = path.join(__dirname, "..", "..", "chrome-extension");
const committed = JSON.parse(fs.readFileSync(path.join(EXTENSION_DIR, "manifest.json"), "utf8"));



test("a base URL saved in config.json replaces the profile's host, and a shared host is listed once", () => {
  const config = { jira: { baseUrl: "https://tracker.mycompany.test/some/path" }, bitbucket: { baseUrl: "https://tracker.mycompany.test" } };
  const hosts = manifestTool.hostPatterns(config);
  assert.ok(hosts.includes("https://tracker.mycompany.test/*"));
  assert.equal(hosts.filter((h) => h === "https://tracker.mycompany.test/*").length, 1);
  assert.ok(!hosts.some((h) => h.includes("jira.example") || h.includes("bitbucket.example")), "the profile's defaults are replaced, not added");
  assert.deepEqual(manifestTool.buildManifest(committed, config).host_permissions.slice(-1), ["http://127.0.0.1/*"]);
});

test("a base URL that isn't http(s) falls out instead of reaching the manifest", () => {
  const hosts = manifestTool.hostPatterns({ jira: { baseUrl: "ftp://nope" }, jenkins: { baseUrl: "not a url" } });
  assert.ok(hosts.every((h) => /^https?:\/\//.test(h)));
});

test("only the generated fields change: name, version, permissions and the worker are kept", () => {
  const base = { ...committed, version: "9.9.9", name: "Renamed" };
  const built = manifestTool.buildManifest(base, { jira: { baseUrl: "https://x.example.com" } });
  assert.equal(built.version, "9.9.9");
  assert.equal(built.name, "Renamed");
  assert.deepEqual(built.permissions, committed.permissions);
  assert.deepEqual(built.background, committed.background);
  assert.equal(built.content_scripts[0].run_at, "document_idle");
});


test("the right-click 'Open in editor' menu is for the git host and CI server, not the issue tracker", () => {
  const matches = manifestTool.contextMenuMatches({});
  assert.deepEqual(matches, [environment.sites.find((s) => s.kind === "git"), environment.sites.find((s) => s.kind === "ci")].map((s) => `${new URL(s.baseUrl).origin}/*`).sort((a, b) => matches.indexOf(a) - matches.indexOf(b)));
  assert.ok(!matches.some((m) => m.includes("jira")));
});

test("originsByKind gives the origin of the git, issues and ci site, honouring config", () => {
  assert.deepEqual(Object.keys(manifestTool.originsByKind({})).sort(), ["ci", "git", "issues"]);
  assert.equal(manifestTool.originsByKind({ bitbucket: { baseUrl: "https://git.example.com/x" } }).git, "https://git.example.com");
});

test("checkManifest names what is wrong with a stale manifest", () => {
  const dir = fs.mkdtempSync(path.join(require("node:os").tmpdir(), "manifest-"));
  try {
    const stale = { ...committed, host_permissions: ["https://old.example.com/*", "http://127.0.0.1/*"] };
    fs.writeFileSync(path.join(dir, "manifest.json"), JSON.stringify(stale));
    const problems = manifestTool.checkManifest(dir, {});
    assert.ok(problems.some((p) => /host_permissions should be/.test(p)), problems.join("\n"));
    assert.ok(problems.some((p) => /is missing from the extension folder/.test(p)));
    assert.equal(manifestTool.writeManifest(dir, {}), true);
    assert.equal(manifestTool.writeManifest(dir, {}), false, "a second write changes nothing");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("every script the manifest lists exists, and every pack feature's script registers that feature", () => {
  for (const script of manifestTool.contentScripts()) assert.ok(fs.existsSync(path.join(EXTENSION_DIR, script)), `${script} is listed but missing`);
  for (const spec of packs.features()) {
    const id = spec.descriptor.id;
    assert.ok(spec.extension && spec.extension.script, `${id} declares no extension script`);
    const source = fs.readFileSync(path.join(EXTENSION_DIR, spec.extension.script), "utf8");
    assert.match(source, /PaiRegistry\.register\(\{/, `${spec.extension.script} never registers`);
    assert.match(source, new RegExp(`^  id: "${id}",`, "m"), `${spec.extension.script} doesn't register "${id}"`);
  }
});


test("every script of the extension compiles (a syntax error would break the whole content-script list)", () => {
  for (const script of [...manifestTool.contentScripts(), "background.js"]) {
    assert.doesNotThrow(() => new vm.Script(fs.readFileSync(path.join(EXTENSION_DIR, script), "utf8"), { filename: script }), script);
  }
});


test("renderExtensionConfig carries the service address, secret and right-click hosts", () => {
  const self = {};
  new Function("self", manifestTool.renderExtensionConfig("s3cret", 4321, ["https://ci.example.com/*"]))(self);
  assert.deepEqual(self.COMPANION_CONFIG, { serviceBaseUrl: "http://127.0.0.1:4321", sharedSecret: "s3cret", contextMenuMatches: ["https://ci.example.com/*"] });
});

test("hostsChanged is true only when the hosts the extension must cover differ", () => {
  assert.equal(manifestTool.hostsChanged({}, {}), false);
  assert.equal(manifestTool.hostsChanged({ jira: { baseUrl: "https://a.example.com" } }, { jira: { baseUrl: "https://a.example.com/other/path" } }), false, "same origin");
  assert.equal(manifestTool.hostsChanged({}, { jira: { baseUrl: "https://a.example.com" } }), true);
  assert.equal(manifestTool.hostsChanged({ jira: { baseUrl: "https://a.example.com" } }, {}), true);
});

test("writeExtensionFiles writes the connection file and brings the manifest up to date", () => {
  const dir = fs.mkdtempSync(path.join(require("node:os").tmpdir(), "ext-files-"));
  try {
    fs.writeFileSync(path.join(dir, "manifest.json"), JSON.stringify(committed));
    const config = { sharedSecret: "abc", port: 8123, jira: { baseUrl: "https://jira.example.com" } };
    assert.equal(manifestTool.writeExtensionFiles(dir, config), true);
    const self = {};
    new Function("self", fs.readFileSync(path.join(dir, "companion-config.js"), "utf8"))(self);
    assert.equal(self.COMPANION_CONFIG.serviceBaseUrl, "http://127.0.0.1:8123");
    assert.equal(self.COMPANION_CONFIG.sharedSecret, "abc");
    assert.ok(JSON.parse(fs.readFileSync(path.join(dir, "manifest.json"), "utf8")).host_permissions.includes("https://jira.example.com/*"));
    assert.equal(manifestTool.writeExtensionFiles(dir, config), false, "nothing to change the second time");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("the default extension folder is the repo's chrome-extension", () => {
  assert.equal(fs.realpathSync(manifestTool.defaultExtensionDir()), fs.realpathSync(EXTENSION_DIR));
});

test("the checked-in manifest is exactly what this profile and its packs generate", () => {
  assert.deepEqual(manifestTool.buildManifest(committed, {}), committed);
  assert.deepEqual(manifestTool.checkManifest(EXTENSION_DIR, {}), []);
  const onDisk = fs.readFileSync(path.join(EXTENSION_DIR, "manifest.json"), "utf8");
  assert.equal(manifestTool.render(committed), onDisk, "formatted as the generator writes it, so setup never dirties the tree");
});
