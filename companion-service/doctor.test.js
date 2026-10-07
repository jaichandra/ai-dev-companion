// doctor.js's background-work lines, with HOME in a temp dir and no network:
// a config without an LLM key never reaches the proxy.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "doctor-home-"));
const { checkBackground, checkPluginPath, checkSimilar } = require("./doctor.js");
const pluginInfo = require("./core/plugin-info.js");

async function lines(config) {
  const out = [];
  const log = console.log;
  console.log = (m) => out.push(String(m));
  try {
    await checkBackground(config);
  } finally {
    console.log = log;
  }
  return out;
}

test("an empty config reports the background work as off and never throws", async () => {
  const out = await lines({});
  const text = out.join("\n");
  assert.match(text, /Background watchers: all off/);
  assert.match(text, /Morning digest: not scheduled/);
  assert.match(text, /LLM proxy: /);
  assert.match(text, /Pre-push check: not installed in any repo/);
  assert.equal(out.every((l) => /^(OK|WARN|FAIL)\s*- /.test(l)), true);
});

test("enabled watchers and quiet hours are shown from the parsed values, not the raw strings", async () => {
  const out = (
    await lines({
      watchers: { conflicts: { enabled: true, intervalMinutes: 15 } },
      scheduler: { quietHours: { start: "19:00", end: "07:00" } },
      digest: { enabled: true, time: "08:30" },
    })
  ).join("\n");
  assert.match(out, /Background watchers: conflicts every 15 min; at most 5 Claude run\(s\) a day/);
  assert.match(out, /Quiet hours: 19:00–07:00/);
  assert.match(out, /Morning digest: weekdays at 08:30/);
  const odd = (await lines({ scheduler: { quietHours: { start: "\u001b[2J", end: "07:00" } }, watchers: 5 })).join("\n");
  assert.doesNotMatch(odd, /\u001b/);
});

test("a surprise inside the check becomes one WARN line instead of stopping doctor", async () => {
  const hostile = {
    get watchers() {
      throw new Error("bad config");
    },
  };
  const out = await lines(hostile);
  assert.equal(out.length, 1);
  assert.match(out[0], /^WARN - Background work: couldn't be checked \(bad config\)/);
});


test("doctor says which way Claude Code reaches the companion: claude mcp add, the plugin, or both", () => {
  const dir = path.join(os.homedir(), ".claude", "plugins");
  fs.mkdirSync(dir, { recursive: true });
  const capture = (claudeState) => {
    const out = [];
    const log = console.log;
    console.log = (m) => out.push(String(m));
    try {
      checkPluginPath(claudeState);
    } finally {
      console.log = log;
    }
    return out.join("\n");
  };
  assert.equal(capture("ok"), "OK   - Claude Code reaches the companion through `claude mcp add` (user scope).");
  assert.equal(capture("missing"), "", "neither path: the registration line above already says so");
  fs.writeFileSync(path.join(dir, "installed_plugins.json"), JSON.stringify({ version: 2, plugins: { "ai-companion@ai-dev-companion": [{}] } }));
  fs.writeFileSync(path.join(os.homedir(), ".claude", "settings.json"), JSON.stringify({ enabledPlugins: { "ai-companion@ai-dev-companion": true } }));
  assert.match(capture("ok"), /^WARN - Claude Code reaches the companion both through `claude mcp add` and the ai-companion plugin/);
  assert.match(capture("missing"), /^OK   - Claude Code reaches the companion through the ai-companion plugin\./);
  fs.rmSync(path.join(os.homedir(), ".claude"), { recursive: true, force: true });
});

function capture(fn) {
  const out = [];
  const log = console.log;
  console.log = (m) => out.push(String(m));
  try {
    fn();
  } finally {
    console.log = log;
  }
  return out.join("\n");
}

test("checkSimilar copes with an empty or hostile config", () => {
  for (const config of [{}, null, undefined, [], { similar: null }, { similar: "off" }, { similar: { enabled: "no" } }]) {
    const text = capture(() => checkSimilar(config, "no-key", "Qwen3-Embedding-8B"));
    assert.match(text, /^(OK|WARN)\s*- Similar past tickets: on/, JSON.stringify(config));
  }
  assert.match(capture(() => checkSimilar({ similar: { enabled: false } }, "ready", "m")), /off/);
  const hostile = capture(() => checkSimilar({}, "ready", "evil\nFAIL - forged"));
  assert.equal(hostile.split("\n").length, 1, "a hostile model name can't add lines");
});

test("checkSimilar counts real vectors only, and tells a corrupt file from an older schema", () => {
  const { openHistory } = require("./core/history-db.js");
  const paths = require("./core/paths.js");
  const file = path.join(paths.stateDir(), "history.db");
  fs.rmSync(file, { force: true });
  const MODEL = "Qwen3-Embedding-8B";
  const h = openHistory(file);
  const a = h.upsertItem({ kind: "ticket", key: "PROJ-1", title: "one" });
  const b = h.upsertItem({ kind: "ticket", key: "PROJ-2", title: "two" });
  h.upsertItem({ kind: "ticket", key: "PROJ-3", title: "three" });
  h.saveVector({ itemId: a, model: MODEL, vector: [1, 0], itemUpdatedAt: 1 });
  h.saveVector({ itemId: b, model: MODEL, vector: [1], itemUpdatedAt: 1 });
  h.close();
  assert.match(capture(() => checkSimilar({}, "ready", MODEL)), /1 of 3 ticket, analysis and PR item\(s\) embedded/, "the placeholder isn't counted");
  fs.writeFileSync(file, "this is not a database, just text that is long enough to be read as one".repeat(20));
  const corrupt = capture(() => checkSimilar({}, "ready", MODEL));
  assert.match(corrupt, /^WARN - Similar past tickets: on, .*couldn't be read/);
  assert.doesNotMatch(corrupt, /next start/);
  fs.rmSync(file, { force: true });
  const { DatabaseSync } = require("node:sqlite");
  const old = new DatabaseSync(file);
  old.exec(require("./core/history-schema.js").MIGRATIONS[0]);
  old.exec("PRAGMA user_version = 1");
  old.close();
  assert.match(capture(() => checkSimilar({}, "ready", MODEL)), /^OK   - Similar past tickets: on, .*older layout.*next start/);
  fs.rmSync(file, { force: true });
});

test("checkPluginPath tolerates malformed and missing Claude Code files, and reports installed-but-disabled", () => {
  const dir = path.join(os.homedir(), ".claude");
  fs.rmSync(dir, { recursive: true, force: true });
  assert.equal(capture(() => checkPluginPath("missing")), "", "no files at all");
  fs.mkdirSync(path.join(dir, "plugins"), { recursive: true });
  fs.writeFileSync(path.join(dir, "plugins", "installed_plugins.json"), "{ not json");
  fs.writeFileSync(path.join(dir, "settings.json"), "[1,2");
  assert.equal(capture(() => checkPluginPath("missing")), "");
  assert.match(capture(() => checkPluginPath("ok")), /^OK   - .*claude mcp add/);
  fs.writeFileSync(path.join(dir, "plugins", "installed_plugins.json"), JSON.stringify({ plugins: null }));
  fs.writeFileSync(path.join(dir, "settings.json"), JSON.stringify({ enabledPlugins: "yes" }));
  assert.equal(capture(() => checkPluginPath("missing")), "");
  fs.writeFileSync(path.join(dir, "plugins", "installed_plugins.json"), JSON.stringify({ plugins: { "ai-companion@x": [{}] } }));
  fs.writeFileSync(path.join(dir, "settings.json"), "{}");
  assert.match(capture(() => checkPluginPath("ok")), /installed but disabled/);
  assert.match(capture(() => checkPluginPath("missing")), /^WARN/);
  fs.writeFileSync(path.join(dir, "settings.json"), JSON.stringify({ enabledPlugins: { "ai-companion@x": true } }));
  const both = capture(() => checkPluginPath("ok"));
  assert.match(both, /^WARN - .*both/);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("plugin detection reads CLAUDE_CONFIG_DIR when set, else ~/.claude", () => {
  assert.equal(pluginInfo.claudeConfigDir({ CLAUDE_CONFIG_DIR: "/x/cfg" }, "/home/me"), "/x/cfg");
  assert.equal(pluginInfo.claudeConfigDir({ CLAUDE_CONFIG_DIR: "  " }, "/home/me"), path.join("/home/me", ".claude"));
  assert.equal(pluginInfo.claudeConfigDir({}, "/home/me"), path.join("/home/me", ".claude"));
  const cfg = fs.mkdtempSync(path.join(os.tmpdir(), "claude-cfg-"));
  fs.mkdirSync(path.join(cfg, "plugins"));
  fs.writeFileSync(path.join(cfg, "plugins", "installed_plugins.json"), JSON.stringify({ plugins: { "ai-companion@x": [{}] } }));
  fs.writeFileSync(path.join(cfg, "settings.json"), JSON.stringify({ enabledPlugins: { "ai-companion@x": true } }));
  process.env.CLAUDE_CONFIG_DIR = cfg;
  try {
    assert.match(capture(() => checkPluginPath("missing")), /^OK   - .*through the ai-companion plugin/);
  } finally {
    delete process.env.CLAUDE_CONFIG_DIR;
    fs.rmSync(cfg, { recursive: true, force: true });
  }
  assert.equal(capture(() => checkPluginPath("missing")), "", "without the variable ~/.claude is read");
});
