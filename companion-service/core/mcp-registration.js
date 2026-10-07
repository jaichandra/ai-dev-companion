// Registers the companion's own MCP server ("ai-companion" — see
// core/mcp-tool-classifier.js's COMPANION_MCP_SERVER, and core/mcp.ts for
// the /mcp route this points at) with the MCP clients that can drive it:
// Claude Code (`claude mcp add --scope user`) always, and Cursor
// (~/.cursor/mcp.json) only when it's actually installed — see the
// standing rule to gate optional integrations on detection. setup.js calls
// registerEverywhere after every run (so a rebuilt port or a rotated
// mcp.token takes effect); server.ts calls it again, synchronously, right
// after a token rotation from the Settings panel (see server.ts's
// reRegisterMcp). doctor.js uses registrationStatus (read-only, no
// spawning) to report drift between runs.
//
// Plain JS, not TypeScript — same reason as the other core/*.js helpers:
// mcp-registration.test.js runs it directly, and setup.js/doctor.js share
// it with zero build step.
const fs = require("fs");
const path = require("path");
const { bearerMatches, generateMcpToken } = require("./mcp-auth.js");
const { COMPANION_MCP_SERVER } = require("./mcp-tool-classifier.js");

// `claude mcp add`'s own timeout budget for a stuck/slow CLI invocation —
// this runs synchronously wherever it's called (setup, and server.ts's
// reRegisterMcp on the rare token-rotation path), so a wedged `claude`
// process must not hang the caller forever.
const SPAWN_TIMEOUT_MS = 20000;

/** The URL Claude Code / Cursor should hit for the companion's own MCP
 * server — always loopback, since the service only ever listens on
 * 127.0.0.1 (see server.ts). */
function mcpUrl(port) {
  return `http://127.0.0.1:${port}/mcp`;
}

/** Removes any existing user-scope entry first, so registration can be
 * re-run (a changed port or a rotated token) without `claude mcp add`
 * refusing to overwrite one that's already there. A missing entry makes
 * this exit non-zero, which callers deliberately ignore. */
function claudeRemoveArgs() {
  return ["mcp", "remove", "--scope", "user", COMPANION_MCP_SERVER];
}

/** `--header` is variadic (`-H, --header <header...>` in `claude mcp add
 * --help`, claude 2.1.281) — it slurps every argument after it, so the
 * name and URL must come first or they'd be swallowed as more headers. */
function claudeAddArgs({ port, token }) {
  return [
    "mcp",
    "add",
    "--scope",
    "user",
    "--transport",
    "http",
    COMPANION_MCP_SERVER,
    mcpUrl(port),
    "--header",
    `Authorization: Bearer ${token}`,
  ];
}

const CURSOR_INVALID_JSON_MESSAGE =
  "~/.cursor/mcp.json isn't valid JSON — fix or remove it, then re-run setup.";
const CURSOR_BAD_MCPSERVERS_MESSAGE =
  "~/.cursor/mcp.json's mcpServers isn't an object — fix or remove it, then re-run setup.";

/** True for an absent `mcpServers` or a plain object; arrays and scalars
 * are not something the companion may merge into. */
function isMissingOrObject(value) {
  if (value === undefined || value === null) return true;
  return typeof value === "object" && !Array.isArray(value);
}

/**
 * Merges the companion's entry into a Cursor mcp.json's text, preserving
 * every other server and top-level key. `existingText` is the file's
 * current contents, or null/"" if it doesn't exist yet (or is being
 * written for the first time) — either produces a fresh file with just
 * this one entry. Throws (rather than silently overwriting) if
 * `existingText` isn't valid JSON: a hand-edited or corrupted mcp.json is
 * the user's, and clobbering it would lose whatever else they'd put there.
 */
function mergeCursorConfig(existingText, { port, token }) {
  let config = {};
  if (existingText !== null && existingText !== undefined && existingText !== "") {
    let parsed;
    try {
      parsed = JSON.parse(existingText);
    } catch {
      throw new Error(CURSOR_INVALID_JSON_MESSAGE);
    }
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      throw new Error(CURSOR_INVALID_JSON_MESSAGE);
    }
    // Spreading an array/string/number here would turn it into index keys
    // and silently rewrite the user's value.
    if (!isMissingOrObject(parsed.mcpServers)) throw new Error(CURSOR_BAD_MCPSERVERS_MESSAGE);
    config = parsed;
  }
  const mcpServers = { ...(config.mcpServers || {}) };
  mcpServers[COMPANION_MCP_SERVER] = {
    url: mcpUrl(port),
    headers: { Authorization: `Bearer ${token}` },
  };
  const next = { ...config, mcpServers };
  return `${JSON.stringify(next, null, 2)}\n`;
}

/** Parses one client's config text for its ai-companion entry: `{entry,
 * unreadable}`. `jsonText` of null/undefined means "no file yet," which is
 * `missing` (an empty entry), not `unreadable` — only text that fails to
 * parse counts as unreadable. Both Claude Code's ~/.claude.json
 * (`{type:"http", url, headers}`) and Cursor's ~/.cursor/mcp.json
 * (`{url, headers}`) keep this at `mcpServers["ai-companion"]`, so one
 * parser covers both. */
function readEntry(jsonText) {
  if (jsonText === null || jsonText === undefined) return { entry: null, unreadable: false };
  let parsed;
  try {
    parsed = JSON.parse(jsonText);
  } catch {
    return { entry: null, unreadable: true };
  }
  if (typeof parsed !== "object" || parsed === null) return { entry: null, unreadable: true };
  if (!isMissingOrObject(parsed.mcpServers)) return { entry: null, unreadable: true };
  return { entry: parsed.mcpServers?.[COMPANION_MCP_SERVER] || null, unreadable: false };
}

/** One client's registration state, given its parsed entry (or lack of
 * one) plus the port/token the companion is actually running with right
 * now. Order matters: unreadable and missing both mean there's nothing
 * trustworthy to compare, so they're decided before url/token are even
 * looked at. */
function entryState(entry, unreadable, port, token) {
  if (unreadable) return "unreadable";
  if (!entry || typeof entry !== "object") return "missing";
  if (entry.url !== mcpUrl(port)) return "wrong-url";
  if (!bearerMatches(entry.headers?.Authorization, token)) return "stale-token";
  return "ok";
}

/**
 * Read-only check of whether Claude Code (always) and Cursor (only when
 * `cursorInstalled`) are registered with the companion's current
 * port/token — used by doctor.js. Never spawns anything or touches disk
 * itself; the caller reads `~/.claude.json` and, if relevant,
 * `~/.cursor/mcp.json` and hands their raw text in.
 */
function registrationStatus({ claudeJsonText, cursorJsonText, cursorInstalled, port, token }) {
  const claude = readEntry(claudeJsonText);
  const results = [{ client: "Claude Code", state: entryState(claude.entry, claude.unreadable, port, token) }];
  if (cursorInstalled) {
    const cursor = readEntry(cursorJsonText);
    results.push({ client: "Cursor", state: entryState(cursor.entry, cursor.unreadable, port, token) });
  }
  return results;
}

/**
 * The `mcp.token` credential (see core/credential-store.js's ALLOWED_NAMES
 * and core/mcp-auth.js's generateMcpToken): whatever's already saved, or a
 * freshly generated one saved on the spot if there's none yet. `generate`
 * is injectable for tests; every real caller uses generateMcpToken.
 */
function ensureMcpToken(store, generate = generateMcpToken) {
  const existing = store.get("mcp.token");
  if (existing) return { token: existing, created: false };
  const token = generate();
  store.set("mcp.token", token);
  return { token, created: true };
}

/** Strips `token` out of `text` wherever it appears, so a CLI's own error
 * output (which can echo back the failing command line, header included)
 * never ends up carrying the secret into a message that setup.js prints
 * or the Settings panel renders. */
function redactToken(text, token) {
  if (!token) return text;
  return text.split(token).join("[redacted]");
}

/** Runs `claude mcp remove` (failure ignored — an absent entry is fine)
 * then `claude mcp add`, and reports the add's outcome. `spawnSync` is
 * injected so tests never shell out to a real `claude` binary. */
function registerWithClaude({ port, token, spawnSync }) {
  spawnSync("claude", claudeRemoveArgs(), { encoding: "utf8", timeout: SPAWN_TIMEOUT_MS });
  const add = spawnSync("claude", claudeAddArgs({ port, token }), { encoding: "utf8", timeout: SPAWN_TIMEOUT_MS });
  if (add && add.status === 0) {
    return { ok: true, message: `Registered with Claude Code (${COMPANION_MCP_SERVER}).` };
  }
  const rawDetail = (add && (add.stderr || add.error?.message)) || "unknown error";
  const detail = redactToken(String(rawDetail).trim(), token) || "unknown error";
  return { ok: false, message: `claude mcp add failed: ${detail}` };
}

/** Merges the companion's entry into ~/.cursor/mcp.json and writes it back
 * atomically (temp file + rename) at mode 0600 — same pattern as
 * core/credential-store.js's writeMap, since this file now carries a
 * bearer token too. `fsImpl` is injected for tests. */
function registerWithCursor({ port, token, home, fsImpl }) {
  const filePath = path.join(home, ".cursor", "mcp.json");
  let existingText = null;
  try {
    existingText = fsImpl.readFileSync(filePath, "utf8");
  } catch (err) {
    if (err.code !== "ENOENT") return { ok: false, message: redactToken(err.message, token) };
  }
  let nextText;
  try {
    nextText = mergeCursorConfig(existingText, { port, token });
  } catch (err) {
    return { ok: false, message: redactToken(err.message, token) };
  }
  const tmp = path.join(home, ".cursor", `.mcp.json.${process.pid}.${Date.now()}.tmp`);
  try {
    fsImpl.writeFileSync(tmp, nextText, { mode: 0o600 });
    fsImpl.renameSync(tmp, filePath);
  } catch (err) {
    try {
      fsImpl.rmSync(tmp, { force: true });
    } catch {
      // Nothing to clean up.
    }
    return { ok: false, message: redactToken(err.message, token) };
  }
  return { ok: true, message: "Registered with Cursor (~/.cursor/mcp.json)." };
}

/**
 * Registers the companion's MCP server with every client that's actually
 * present on this machine — the standing rule to gate optional
 * integrations on detection: Claude Code only when `claudeAvailable`
 * (checked by the caller via core/prereqs.js's checkClaudeCli), Cursor
 * only when `~/.cursor` exists. Returns `{claude, cursor}`, each either
 * `{ok, message}` or null when that client isn't present — never the
 * token in any message, since both setup.js's console output and the
 * Settings panel show these as-is.
 */
function registerEverywhere({ port, token, home, claudeAvailable, spawnSync, fsImpl = fs }) {
  const claude = claudeAvailable ? registerWithClaude({ port, token, spawnSync }) : null;
  const cursorDir = path.join(home, ".cursor");
  const cursor = fsImpl.existsSync(cursorDir) ? registerWithCursor({ port, token, home, fsImpl }) : null;
  return { claude, cursor };
}

module.exports = {
  mcpUrl,
  claudeRemoveArgs,
  claudeAddArgs,
  mergeCursorConfig,
  registrationStatus,
  ensureMcpToken,
  registerEverywhere,
};
