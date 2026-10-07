const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const {
  mcpUrl,
  claudeRemoveArgs,
  claudeAddArgs,
  mergeCursorConfig,
  registrationStatus,
  ensureMcpToken,
  registerEverywhere,
} = require("./mcp-registration.js");

function tmpRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "mcp-registration-test-"));
}

/** Records every call and returns canned results in order (the last one
 * repeats if more calls come in than results provided — no test here
 * needs that, but it keeps a stray extra call from throwing an unrelated
 * error). */
function fakeSpawnSync(results) {
  const calls = [];
  let i = 0;
  const fn = (command, args, options) => {
    calls.push({ command, args, options });
    const result = results[Math.min(i, results.length - 1)];
    i++;
    return result;
  };
  return { calls, fn };
}

// ---- mcpUrl / claudeRemoveArgs / claudeAddArgs ----

test("mcpUrl builds the loopback /mcp URL for a port", () => {
  assert.equal(mcpUrl(8787), "http://127.0.0.1:8787/mcp");
});

test("claudeRemoveArgs removes the user-scope entry", () => {
  assert.deepEqual(claudeRemoveArgs(), ["mcp", "remove", "--scope", "user", "ai-companion"]);
});

test("claudeAddArgs puts --header last, after the name and url", () => {
  assert.deepEqual(claudeAddArgs({ port: 8787, token: "tok-value" }), [
    "mcp",
    "add",
    "--scope",
    "user",
    "--transport",
    "http",
    "ai-companion",
    "http://127.0.0.1:8787/mcp",
    "--header",
    "Authorization: Bearer tok-value",
  ]);
});

// ---- mergeCursorConfig ----

test("mergeCursorConfig keeps other servers and unrelated top-level keys", () => {
  const existing = JSON.stringify({
    mcpServers: { other: { url: "http://example.test/mcp" } },
    someOtherKey: "kept",
  });
  const parsed = JSON.parse(mergeCursorConfig(existing, { port: 8787, token: "tok" }));
  assert.deepEqual(parsed.mcpServers.other, { url: "http://example.test/mcp" });
  assert.equal(parsed.someOtherKey, "kept");
  assert.deepEqual(parsed.mcpServers["ai-companion"], {
    url: "http://127.0.0.1:8787/mcp",
    headers: { Authorization: "Bearer tok" },
  });
});

test("mergeCursorConfig produces a fresh file for null or empty input", () => {
  for (const input of [null, ""]) {
    const parsed = JSON.parse(mergeCursorConfig(input, { port: 8787, token: "tok" }));
    assert.deepEqual(parsed, {
      mcpServers: { "ai-companion": { url: "http://127.0.0.1:8787/mcp", headers: { Authorization: "Bearer tok" } } },
    });
  }
});

test("mergeCursorConfig throws on unparsable input instead of clobbering the file", () => {
  assert.throws(
    () => mergeCursorConfig("{not json", { port: 8787, token: "tok" }),
    /~\/\.cursor\/mcp\.json isn't valid JSON — fix or remove it, then re-run setup\./,
  );
});

test("mergeCursorConfig is idempotent", () => {
  const once = mergeCursorConfig(null, { port: 8787, token: "tok" });
  const twice = mergeCursorConfig(once, { port: 8787, token: "tok" });
  assert.equal(once, twice);
});

// ---- registrationStatus ----

test("registrationStatus reports ok when the url and token match", () => {
  const claudeJsonText = JSON.stringify({
    mcpServers: { "ai-companion": { type: "http", url: mcpUrl(8787), headers: { Authorization: "Bearer tok" } } },
  });
  assert.deepEqual(
    registrationStatus({ claudeJsonText, cursorJsonText: null, cursorInstalled: false, port: 8787, token: "tok" }),
    [{ client: "Claude Code", state: "ok" }],
  );
});

test("registrationStatus reports missing when there's no entry, including a missing file", () => {
  assert.deepEqual(
    registrationStatus({
      claudeJsonText: JSON.stringify({}),
      cursorJsonText: null,
      cursorInstalled: false,
      port: 8787,
      token: "tok",
    }),
    [{ client: "Claude Code", state: "missing" }],
  );
  assert.deepEqual(
    registrationStatus({ claudeJsonText: null, cursorJsonText: null, cursorInstalled: false, port: 8787, token: "tok" }),
    [{ client: "Claude Code", state: "missing" }],
  );
});

test("registrationStatus reports stale-token when the header carries a different token", () => {
  const claudeJsonText = JSON.stringify({
    mcpServers: { "ai-companion": { type: "http", url: mcpUrl(8787), headers: { Authorization: "Bearer old-token" } } },
  });
  assert.deepEqual(
    registrationStatus({
      claudeJsonText,
      cursorJsonText: null,
      cursorInstalled: false,
      port: 8787,
      token: "new-token",
    }),
    [{ client: "Claude Code", state: "stale-token" }],
  );
});

test("registrationStatus reports wrong-url when the port differs", () => {
  const claudeJsonText = JSON.stringify({
    mcpServers: { "ai-companion": { type: "http", url: mcpUrl(9999), headers: { Authorization: "Bearer tok" } } },
  });
  assert.deepEqual(
    registrationStatus({ claudeJsonText, cursorJsonText: null, cursorInstalled: false, port: 8787, token: "tok" }),
    [{ client: "Claude Code", state: "wrong-url" }],
  );
});

test("registrationStatus reports unreadable on invalid JSON", () => {
  assert.deepEqual(
    registrationStatus({
      claudeJsonText: "{not json",
      cursorJsonText: null,
      cursorInstalled: false,
      port: 8787,
      token: "tok",
    }),
    [{ client: "Claude Code", state: "unreadable" }],
  );
});

test("registrationStatus omits Cursor when it isn't installed", () => {
  const cursorJsonText = JSON.stringify({
    mcpServers: { "ai-companion": { url: mcpUrl(8787), headers: { Authorization: "Bearer tok" } } },
  });
  const result = registrationStatus({
    claudeJsonText: null,
    cursorJsonText,
    cursorInstalled: false,
    port: 8787,
    token: "tok",
  });
  assert.deepEqual(result, [{ client: "Claude Code", state: "missing" }]);
});

test("registrationStatus includes Cursor when installed", () => {
  const cursorJsonText = JSON.stringify({
    mcpServers: { "ai-companion": { url: mcpUrl(8787), headers: { Authorization: "Bearer tok" } } },
  });
  const result = registrationStatus({
    claudeJsonText: null,
    cursorJsonText,
    cursorInstalled: true,
    port: 8787,
    token: "tok",
  });
  assert.deepEqual(result, [
    { client: "Claude Code", state: "missing" },
    { client: "Cursor", state: "ok" },
  ]);
});

// ---- ensureMcpToken ----

test("ensureMcpToken keeps an existing token", () => {
  const store = {
    get: (name) => (name === "mcp.token" ? "existing-token" : undefined),
    set: () => {
      throw new Error("should not save when a token already exists");
    },
  };
  assert.deepEqual(ensureMcpToken(store, () => "unused"), { token: "existing-token", created: false });
});

test("ensureMcpToken creates and saves exactly one token when missing", () => {
  const sets = [];
  const store = { get: () => undefined, set: (name, value) => sets.push([name, value]) };
  const result = ensureMcpToken(store, () => "generated-token");
  assert.deepEqual(result, { token: "generated-token", created: true });
  assert.deepEqual(sets, [["mcp.token", "generated-token"]]);
});

// ---- registerEverywhere ----

test("registerEverywhere returns claude: null and never spawns when claudeAvailable is false", () => {
  const spawn = fakeSpawnSync([{ status: 0, stdout: "", stderr: "" }]);
  const home = tmpRoot();
  try {
    const result = registerEverywhere({
      port: 8787,
      token: "tok-value",
      home,
      claudeAvailable: false,
      spawnSync: spawn.fn,
    });
    assert.equal(result.claude, null);
    assert.equal(spawn.calls.length, 0);
    assert.equal(result.cursor, null);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("registerEverywhere runs remove then add, ignoring a failing remove, with the documented spawnSync options", () => {
  const spawn = fakeSpawnSync([
    { status: 1, stdout: "", stderr: "no such server" }, // remove: absent entry is fine
    { status: 0, stdout: "", stderr: "" }, // add
  ]);
  const home = tmpRoot();
  try {
    const result = registerEverywhere({
      port: 8787,
      token: "tok-value",
      home,
      claudeAvailable: true,
      spawnSync: spawn.fn,
    });
    assert.equal(spawn.calls.length, 2);
    assert.equal(spawn.calls[0].command, "claude");
    assert.deepEqual(spawn.calls[0].args, claudeRemoveArgs());
    assert.equal(spawn.calls[1].command, "claude");
    assert.deepEqual(spawn.calls[1].args, claudeAddArgs({ port: 8787, token: "tok-value" }));
    for (const call of spawn.calls) {
      assert.deepEqual(call.options, { encoding: "utf8", timeout: 20000 });
    }
    assert.equal(result.claude.ok, true);
    assert.ok(!result.claude.message.includes("tok-value"));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("registerEverywhere redacts the token from a failing add's message", () => {
  const token = "super-secret-token-value";
  const spawn = fakeSpawnSync([
    { status: 0, stdout: "", stderr: "" },
    { status: 1, stdout: "", stderr: `  error: invalid header "Authorization: Bearer ${token}"\n` },
  ]);
  const home = tmpRoot();
  try {
    const result = registerEverywhere({ port: 8787, token, home, claudeAvailable: true, spawnSync: spawn.fn });
    assert.equal(result.claude.ok, false);
    assert.ok(!result.claude.message.includes(token), `message leaked the token: ${result.claude.message}`);
    assert.ok(result.claude.message.length > 0);
    assert.ok(result.cursor === null || !result.cursor.message.includes(token));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("registerEverywhere returns cursor: null when ~/.cursor doesn't exist", () => {
  const home = tmpRoot();
  try {
    const result = registerEverywhere({
      port: 8787,
      token: "tok-value",
      home,
      claudeAvailable: false,
      spawnSync: () => {
        throw new Error("should not spawn");
      },
    });
    assert.equal(result.cursor, null);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("registerEverywhere writes ~/.cursor/mcp.json mode 0600 when the dir exists", () => {
  const home = tmpRoot();
  try {
    fs.mkdirSync(path.join(home, ".cursor"));
    const result = registerEverywhere({
      port: 8787,
      token: "tok-value",
      home,
      claudeAvailable: false,
      spawnSync: () => {
        throw new Error("should not spawn");
      },
    });
    assert.equal(result.cursor.ok, true);
    assert.ok(!result.cursor.message.includes("tok-value"));
    const filePath = path.join(home, ".cursor", "mcp.json");
    const stat = fs.statSync(filePath);
    assert.equal(stat.mode & 0o777, 0o600);
    const written = JSON.parse(fs.readFileSync(filePath, "utf8"));
    assert.deepEqual(written.mcpServers["ai-companion"], {
      url: mcpUrl(8787),
      headers: { Authorization: "Bearer tok-value" },
    });
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("registerEverywhere merges into an existing ~/.cursor/mcp.json without dropping other servers", () => {
  const home = tmpRoot();
  try {
    fs.mkdirSync(path.join(home, ".cursor"));
    fs.writeFileSync(
      path.join(home, ".cursor", "mcp.json"),
      JSON.stringify({ mcpServers: { other: { url: "http://example.test/mcp" } } }),
    );
    const result = registerEverywhere({
      port: 8787,
      token: "tok-value",
      home,
      claudeAvailable: false,
      spawnSync: () => {
        throw new Error("should not spawn");
      },
    });
    assert.equal(result.cursor.ok, true);
    const written = JSON.parse(fs.readFileSync(path.join(home, ".cursor", "mcp.json"), "utf8"));
    assert.deepEqual(written.mcpServers.other, { url: "http://example.test/mcp" });
    assert.deepEqual(written.mcpServers["ai-companion"], {
      url: mcpUrl(8787),
      headers: { Authorization: "Bearer tok-value" },
    });
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// ---- non-object mcpServers in an existing Cursor config ----

for (const [label, bad] of [
  ["an array", ["x"]],
  ["a string", "oops"],
  ["a number", 5],
]) {
  test(`mergeCursorConfig throws when mcpServers is ${label}, rather than rewriting it`, () => {
    assert.throws(
      () => mergeCursorConfig(JSON.stringify({ mcpServers: bad }), { port: 8787, token: "t" }),
      /mcp\.json.*fix or remove it, then re-run setup/,
    );
  });

  test(`registrationStatus reports Cursor as unreadable when mcpServers is ${label}`, () => {
    const results = registrationStatus({
      claudeJsonText: null,
      cursorJsonText: JSON.stringify({ mcpServers: bad }),
      cursorInstalled: true,
      port: 8787,
      token: "t",
    });
    assert.equal(results.find((r) => r.client === "Cursor").state, "unreadable");
  });
}

test("registerEverywhere leaves a Cursor config with a non-object mcpServers byte-for-byte unchanged", () => {
  const home = tmpRoot();
  try {
    fs.mkdirSync(path.join(home, ".cursor"));
    const file = path.join(home, ".cursor", "mcp.json");
    const original = '{"mcpServers":["keep","me"],"other":1}';
    fs.writeFileSync(file, original);
    const result = registerEverywhere({
      port: 8787,
      token: "tok-value",
      home,
      claudeAvailable: false,
      spawnSync: () => {
        throw new Error("should not spawn");
      },
    });
    assert.equal(result.cursor.ok, false);
    assert.equal(fs.readFileSync(file, "utf8"), original);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
