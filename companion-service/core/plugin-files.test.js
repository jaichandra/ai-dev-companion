// The Claude Code plugin under plugin/ and the repo's marketplace entry,
// checked structurally (claude isn't run here), plus core/plugin-info.js.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const info = require("./plugin-info.js");
const { COMPANION_MCP_SERVER } = require("./mcp-tool-classifier.js");

const ROOT = path.join(__dirname, "..", "..");
const PLUGIN = path.join(ROOT, "plugin");
const readJson = (...p) => JSON.parse(fs.readFileSync(path.join(...p), "utf8"));
const rootVersion = readJson(ROOT, "package.json").version;
const SKILLS = ["companion-status", "resume-ticket", "pre-push-check"];

function frontmatter(text) {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n/.exec(text);
  assert.ok(m, "a SKILL.md starts with --- frontmatter ---");
  const out = {};
  for (const line of m[1].split(/\r?\n/)) {
    const kv = /^([a-z-]+):\s*(.*)$/.exec(line);
    if (kv) out[kv[1]] = kv[2];
  }
  return out;
}



test("each skill has a name matching its folder, a description, bounded tools, and says when the service isn't running", () => {
  const dirs = fs.readdirSync(path.join(PLUGIN, "skills")).sort();
  assert.deepEqual(dirs, [...SKILLS].sort());
  for (const name of SKILLS) {
    const text = fs.readFileSync(path.join(PLUGIN, "skills", name, "SKILL.md"), "utf8");
    const fm = frontmatter(text);
    assert.equal(fm.name, name);
    assert.ok(fm.description && fm.description.length >= 60, `${name}: description`);
    assert.ok(fm["allowed-tools"], `${name}: allowed-tools`);
    for (const tool of fm["allowed-tools"].split(/,\s*/)) assert.match(tool, /^Bash\((companion|git) [a-z -]+:\*\)$/, `${name}: ${tool}`);
    assert.match(text, /companion status/, `${name}: checks the service first`);
    assert.match(text, /node ~\/ai-dev-companion\/companion-service\/bin\/companion\.js doctor/, `${name}: says how to run doctor when companion isn't on PATH`);
    assert.match(text, /"companion service not running"/, `${name}: says so instead of failing silently`);
    assert.match(text, /untrusted|uses no AI/, `${name}: treats fetched text as data`);
  }
  assert.doesNotMatch(fs.readFileSync(path.join(PLUGIN, "skills", "resume-ticket", "SKILL.md"), "utf8"), /allowed-tools:.*companion resume/, "resume-ticket never runs an interactive session itself");
});

test("the SessionStart hook runs the inbox line from the plugin root with a short timeout", () => {
  const hooks = readJson(PLUGIN, "hooks", "hooks.json").hooks;
  assert.deepEqual(Object.keys(hooks), ["SessionStart"]);
  const [entry] = hooks.SessionStart;
  assert.equal(entry.hooks.length, 1);
  assert.deepEqual(entry.hooks[0], { type: "command", command: 'node "${CLAUDE_PLUGIN_ROOT}/hooks/inbox-brief.js"', timeout: 5 });
});

test("without the companion installed, the launcher says so and exits 1, and the hook stays silent", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "plugin-home-"));
  try {
    const env = { ...process.env, HOME: home, USERPROFILE: home };
    const mcp = spawnSync(process.execPath, [path.join(PLUGIN, "bin", "ai-companion-mcp.js")], { env, encoding: "utf8", input: "" });
    assert.equal(mcp.status, 1);
    assert.match(mcp.stderr, /companion service isn't installed/);
    assert.equal(mcp.stdout, "");
    const hook = spawnSync(process.execPath, [path.join(PLUGIN, "hooks", "inbox-brief.js")], { env, encoding: "utf8" });
    assert.equal(hook.status, 0);
    assert.equal(hook.stdout + hook.stderr, "");
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("pluginInstallText prints the /plugin lines for the installed copy (and the shared repo), and how to avoid duplicates", () => {
  const text = info.pluginInstallText({ stableDir: "/Users/me/ai-dev-companion", updateUrl: "https://bb.example/scm/x/ai-dev-companion.git" });
  assert.match(text, /^  \/plugin marketplace add \/Users\/me\/ai-dev-companion$/m);
  assert.match(text, /^  \/plugin install ai-companion@ai-dev-companion$/m);
  assert.match(text, /^  \/plugin marketplace add https:\/\/bb\.example\/scm\/x\/ai-dev-companion\.git$/m);
  const pinned = info.pluginInstallText({ stableDir: "/d", updateUrl: "https://bb.example/scm/x/a.git", updateRef: "release" });
  assert.match(pinned, /^  \/plugin marketplace add https:\/\/bb\.example\/scm\/x\/a\.git#release$/m);
  assert.match(text, /claude mcp remove --scope user ai-companion/);
  assert.doesNotMatch(info.pluginInstallText({ stableDir: "/d", updateUrl: "git@x:y.git" }), /git@x/);
});

test("pluginStatus and mcpPathSummary report which way Claude Code reaches the companion", () => {
  const installedPluginsText = JSON.stringify({ version: 2, plugins: { "ai-companion@ai-dev-companion": [{}], "other@x": [{}] } });
  const on = info.pluginStatus({ installedPluginsText, settingsText: JSON.stringify({ enabledPlugins: { "ai-companion@ai-dev-companion": true } }) });
  assert.deepEqual(on, { installed: true, enabled: true, ids: ["ai-companion@ai-dev-companion"] });
  const off = info.pluginStatus({ installedPluginsText, settingsText: "{}" });
  assert.deepEqual(off, { installed: true, enabled: false, ids: ["ai-companion@ai-dev-companion"] });
  assert.deepEqual(info.pluginStatus({ installedPluginsText: "not json", settingsText: null }), { installed: false, enabled: false, ids: [] });
  assert.equal(info.mcpPathSummary({ claudeState: "ok", plugin: on }).level, "WARN");
  assert.match(info.mcpPathSummary({ claudeState: "ok", plugin: on }).fix, /claude mcp remove/);
  assert.match(info.mcpPathSummary({ claudeState: "missing", plugin: on }).message, /through the ai-companion plugin/);
  assert.match(info.mcpPathSummary({ claudeState: "ok", plugin: off }).message, /claude mcp add.*installed but disabled/);
  assert.equal(info.mcpPathSummary({ claudeState: "missing", plugin: off }).level, "WARN");
  assert.equal(info.mcpPathSummary({ claudeState: "missing", plugin: { installed: false, enabled: false } }), null);
});

test("frontmatter reading tolerates CRLF line endings", () => {
  const fm = frontmatter("---\r\nname: x\r\ndescription: a b\r\n---\r\nbody\r\n");
  assert.deepEqual(fm, { name: "x", description: "a b" });
});

/** Runs the SessionStart hook with a fake installed companion.js (its source given). */
function runHook(companionSource) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "plugin-hook-"));
  try {
    const dir = path.join(home, "ai-dev-companion", "companion-service", "bin");
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "companion.js"), companionSource);
    const started = Date.now();
    const r = spawnSync(process.execPath, [path.join(PLUGIN, "hooks", "inbox-brief.js")], {
      env: { ...process.env, HOME: home, USERPROFILE: home },
      encoding: "utf8",
      timeout: 15000,
    });
    return { status: r.status, out: r.stdout, err: r.stderr, ms: Date.now() - started };
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
}

test("the SessionStart hook prints only the inbox line, and is silent and fast whatever the companion does", () => {
  const line = runHook('console.log("AI companion: 2 finished, 1 needs you\\nmore lines");');
  assert.equal(line.status, 0);
  assert.equal(line.out, "AI companion: 2 finished, 1 needs you\n", "just the first line");
  const other = runHook('console.log("something else");');
  assert.equal(other.status, 0);
  assert.equal(other.out + other.err, "");
  const failing = runHook('console.log("AI companion: nope"); process.exit(3);');
  assert.equal(failing.status, 0);
  assert.equal(failing.out + failing.err, "", "a failing companion adds nothing");
  const slow = runHook("setTimeout(() => console.log('AI companion: late'), 20000);");
  assert.equal(slow.status, 0);
  assert.equal(slow.out + slow.err, "");
  assert.ok(slow.ms < 5000, `a slow companion is cut off (${slow.ms} ms)`);
  const broken = runHook("this is not javascript (");
  assert.equal(broken.status, 0);
  assert.equal(broken.out, "");
});
