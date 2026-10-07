const test = require("node:test");
const assert = require("node:assert/strict");
const os = require("os");
const path = require("path");
const launchd = require("./launchd.js");

test("plistPath lives under ~/Library/LaunchAgents and is named after LABEL", () => {
  const result = launchd.plistPath();
  assert.equal(result, path.join(os.homedir(), "Library", "LaunchAgents", `${launchd.LABEL}.plist`));
});

test("logPaths point at two distinct files under the companion-service dir", () => {
  const { out, err } = launchd.logPaths();
  assert.notEqual(out, err);
  assert.match(out, /companion-service\.out\.log$/);
  assert.match(err, /companion-service\.err\.log$/);
});

test("serverEntryPath points at dist/server.js", () => {
  assert.match(launchd.serverEntryPath(), /dist[\\/]server\.js$/);
});

test("renderPlist includes the node binary and server script as ProgramArguments", () => {
  const xml = launchd.renderPlist({
    label: "com.example.test",
    nodePath: "/usr/local/bin/node",
    serverPath: "/Users/x/companion-service/dist/server.js",
    workingDir: "/Users/x/companion-service",
    pathEnv: "/usr/local/bin:/usr/bin:/bin",
    outLog: "/Users/x/companion-service/companion-service.out.log",
    errLog: "/Users/x/companion-service/companion-service.err.log",
  });
  assert.match(xml, /<string>com\.example\.test<\/string>/);
  assert.match(xml, /<string>\/usr\/local\/bin\/node<\/string>/);
  assert.match(xml, /<string>\/Users\/x\/companion-service\/dist\/server\.js<\/string>/);
  assert.match(xml, /<string>\/Users\/x\/companion-service<\/string>/);
  assert.match(xml, /<string>\/usr\/local\/bin:\/usr\/bin:\/bin<\/string>/);
  assert.match(xml, /companion-service\.out\.log/);
  assert.match(xml, /companion-service\.err\.log/);
  assert.match(xml, /<key>RunAtLoad<\/key>\s*<true\/>/);
  assert.match(xml, /<key>KeepAlive<\/key>\s*<dict>\s*<key>SuccessfulExit<\/key>\s*<false\/>/);
});

test("renderPlist starts the service with the system certificate store (Acme's proxy certificate)", () => {
  const xml = launchd.renderPlist({
    label: "com.example.test",
    nodePath: "/usr/local/bin/node",
    serverPath: "/s.js",
    workingDir: "/w",
    pathEnv: "/usr/bin",
    outLog: "/o.log",
    errLog: "/e.log",
  });
  assert.match(xml, /<key>NODE_USE_SYSTEM_CA<\/key>\s*<string>1<\/string>/);
});

test("buildServicePath drops npm/npx-only entries and appends missing system dirs", () => {
  const result = launchd.buildServicePath(
    [
      "/Users/x/ai-dev-companion/companion-service/node_modules/.bin",
      "/Users/x/.nvm/versions/node/v24/lib/node_modules/npm/node_modules/@npmcli/run-script/lib/node-gyp-bin",
      "/Users/x/.npm/_npx/abc123/node_modules/.bin",
      "/Users/x/.nvm/versions/node/v24/bin",
      "/usr/bin",
    ].join(":"),
  );
  const dirs = result.split(":");
  assert.equal(dirs[0], "/Users/x/.nvm/versions/node/v24/bin");
  assert.ok(!dirs.some((d) => /node_modules|_npx/.test(d)));
  assert.equal(dirs.filter((d) => d === "/usr/bin").length, 1);
  for (const dir of ["/usr/local/bin", "/opt/homebrew/bin", "/bin"]) assert.ok(dirs.includes(dir));
});

test("buildServicePath tolerates an empty PATH", () => {
  assert.match(launchd.buildServicePath(undefined), /^\/opt\/homebrew\/bin:.*:\/sbin$/);
});

test("renderPlist escapes XML special characters in path values", () => {
  const xml = launchd.renderPlist({
    label: "com.example.test",
    nodePath: "/usr/local/bin/node",
    serverPath: "/Users/x & y/dist/server.js",
    workingDir: "/Users/x & y",
    pathEnv: "/usr/bin",
    outLog: "/tmp/out.log",
    errLog: "/tmp/err.log",
  });
  assert.match(xml, /\/Users\/x &amp; y/);
  assert.doesNotMatch(xml, /\/Users\/x & y/);
});

test("renderPlist produces well-formed-looking XML (balanced dict/array tags)", () => {
  const xml = launchd.renderPlist({
    label: "com.example.test",
    nodePath: "/usr/local/bin/node",
    serverPath: "/x/dist/server.js",
    workingDir: "/x",
    pathEnv: "/usr/bin",
    outLog: "/tmp/out.log",
    errLog: "/tmp/err.log",
  });
  // <(tag ...)?> — allows an opening tag's attributes, e.g. <plist version="1.0">.
  const opens = (tag) => (xml.match(new RegExp(`<${tag}(\\s[^>]*)?>`, "g")) || []).length;
  const closes = (tag) => (xml.match(new RegExp(`</${tag}>`, "g")) || []).length;
  for (const tag of ["dict", "array", "plist"]) {
    assert.equal(opens(tag), closes(tag), `${tag} open/close tags should balance`);
  }
});
