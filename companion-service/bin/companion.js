#!/usr/bin/env node
"use strict";
// The `companion` command. Plain JS, no build step, no npm dependencies — it
// runs straight from the installed copy. All behaviour is in core/cli-run.js.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn, spawnSync } = require("child_process");
const paths = require("../core/paths.js");
const { gitEnv } = require("../core/git-hook.js");
const { createCredentialStore } = require("../core/credential-store.js");
const { callTool } = require("../core/mcp-client.js");
const { main } = require("../core/cli-run.js");
const { sessionCwdAllowed } = require("../core/session-cwd.js");
const { createStdioBridge, runStdio } = require("../core/mcp-stdio.js");

const companionDir = path.join(__dirname, "..");

function readConfig() {
  try {
    return JSON.parse(fs.readFileSync(path.join(companionDir, "config.json"), "utf8"));
  } catch {
    return null;
  }
}

function readPort() {
  return (readConfig() || {}).port || 8787;
}

// The pre-push check stops waiting after about 3 seconds (git and the service
// call have their own short timeouts) and never holds a push up: it always exits 0.
const argv = process.argv.slice(2);
const isPrecheck = argv[0] === "precheck";
// `inbox --brief` (the SessionStart hook) is advisory in the same way: silent on
// any failure, always exit 0, never left waiting on a slow service.
const isBrief = argv[0] === "inbox" && argv.includes("--brief");

function readStdin() {
  return new Promise((resolve) => {
    let text = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      if (text.length < 64 * 1024) text += chunk;
    });
    process.stdin.on("end", () => resolve(text));
    process.stdin.on("error", () => resolve(text));
  });
}

/** `git` with an argv array (never a shell); a short timeout for the pre-push check. */
function git(args, cwd, opts) {
  const r = spawnSync("git", args, {
    cwd,
    encoding: "utf8",
    timeout: isPrecheck ? 2500 : 30000,
    maxBuffer: 16 * 1024 * 1024,
    env: gitEnv(process.env, { isolated: Boolean(opts && opts.isolated) }),
  });
  return Promise.resolve({ code: r.status === null ? -1 : r.status, stdout: r.stdout || "", stderr: r.stderr || "" });
}

// A closed stderr/stdout (EPIPE) must not turn an advisory check into a failure.
if (isPrecheck) {
  process.stdout.on("error", () => {});
  process.stderr.on("error", () => {});
}

const port = readPort();

function readRepoPaths() {
  try {
    const repos = JSON.parse(fs.readFileSync(path.join(companionDir, "config.json"), "utf8")).repos;
    return repos && typeof repos === "object" ? Object.values(repos) : [];
  } catch {
    return [];
  }
}

function readToken() {
  const store = createCredentialStore({
    filePath: path.join(companionDir, "credentials.enc"),
    keyPath: path.join(paths.stateDir(), "credentials.key"),
  });
  return store.get("mcp.token");
}

function run(command, args, cwd) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { cwd, stdio: "inherit" });
    child.on("error", (err) => {
      process.stderr.write(
        err.code === "ENOENT"
          ? `companion: \`${command}\` wasn't found on your PATH.\n`
          : `companion: couldn't start ${command}: ${err.message}\n`,
      );
      resolve(1);
    });
    child.on("exit", (code) => resolve(code ?? 1));
  });
}

/** `companion mcp-stdio` (the Claude Code plugin's MCP server): JSON-RPC
 * lines in on stdin, answers out on stdout, one at a time, through the
 * service's own /mcp (core/mcp-stdio.js). Nothing else may be written to stdout. */
function runMcpStdio() {
  const bridge = createStdioBridge({ url: `http://127.0.0.1:${port}/mcp`, getToken: readToken });
  runStdio({ stdin: process.stdin, stdout: process.stdout, stderr: process.stderr, bridge }).then((code) => process.exit(code));
}

if (argv[0] === "mcp-stdio") {
  runMcpStdio();
} else main(argv, {
  baseUrl: `http://127.0.0.1:${port}`,
  now: Date.now,
  home: os.homedir(),
  out: (s) => process.stdout.write(s),
  err: (s) => process.stderr.write(s),
  exists: (p) => fs.existsSync(p),
  cwdAllowed: (cwd) => sessionCwdAllowed(cwd, { repoPaths: readRepoPaths(), sessionsRoot: path.join(paths.stateDir(), "sessions") }),
  callTool: async (name, args) => {
    const token = readToken();
    if (!token) {
      throw new Error("No MCP token is saved yet. Run `npm run setup` in the companion-service folder, then try again.");
    }
    return callTool({ url: `http://127.0.0.1:${port}/mcp`, token, name, args, timeoutMs: isPrecheck ? 2500 : isBrief ? 3000 : 60000 });
  },
  env: process.env,
  cwd: process.cwd(),
  resolve: path.resolve,
  relative: path.relative,
  readConfig,
  readStdin,
  git,
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  mkTemp: () => fs.mkdtempSync(path.join(os.tmpdir(), "companion-hook-probe-")),
  rmTemp: (dir) => fs.rmSync(dir, { recursive: true, force: true }),
  runClaude: (args, cwd) => run("claude", args, cwd),
  runDoctor: () => run(process.execPath, [path.join(companionDir, "doctor.js")], companionDir),
}).then((code) => {
  // The pre-push check is advisory: it exits 0 whatever `code` says.
  process.exitCode = isPrecheck || isBrief ? 0 : code;
  // Nothing left pending (a slow service, stdin) may keep a push waiting.
  if (isPrecheck || isBrief) process.exit(0);
}, (err) => {
  // The pre-push check is advisory: whatever went wrong, the push goes on.
  if (isPrecheck || isBrief) process.exit(0);
  console.error(err && err.message ? err.message : err);
  process.exit(1);
});
