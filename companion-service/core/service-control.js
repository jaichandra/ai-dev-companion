// Build / health-check helpers shared by setup.js (which builds, starts and
// verifies the service at the end of every run) and doctor.js (which
// diagnoses a service that isn't working). Plain JS for the same reason as
// core/prereqs.js: both callers run before any build exists.
const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const COMPANION_SERVICE_DIR = path.join(__dirname, "..");

/** Compiles the TypeScript sources into dist/ with the locally installed
 * tsc. Returns { ok, output }. */
function buildServer() {
  const tscPath = path.join(COMPANION_SERVICE_DIR, "node_modules", "typescript", "bin", "tsc");
  if (!fs.existsSync(tscPath)) {
    return {
      ok: false,
      output: `TypeScript isn't installed in ${COMPANION_SERVICE_DIR} — run "npm install --include=dev" there first.`,
    };
  }
  const result = spawnSync(process.execPath, [tscPath, "-p", "."], {
    cwd: COMPANION_SERVICE_DIR,
    encoding: "utf8",
  });
  const output = `${result.stdout || ""}${result.stderr || ""}`.trim();
  return { ok: result.status === 0, output: output || (result.error ? result.error.message : "") };
}

/**
 * Probes the running service. `state` is one of:
 *   "down"          nothing answering on the port
 *   "unauthorized"  something answers, but rejects this secret (the
 *                   extension would get the same 403)
 *   "ok"            reachable and the secret works; `features` lists what's enabled
 *   "error"         reachable but answered unexpectedly
 */
async function checkHealth(port, secret) {
  const base = `http://127.0.0.1:${port}`;
  let res;
  try {
    res = await fetch(`${base}/features`, {
      headers: { "X-Companion-Secret": secret },
      signal: AbortSignal.timeout(2000),
    });
  } catch {
    return { state: "down", message: `Nothing is answering on ${base}.` };
  }
  if (res.status === 403) {
    return {
      state: "unauthorized",
      message: `${base} is answering but rejects the configured secret — it's an old copy of the service, or another program.`,
    };
  }
  if (!res.ok) {
    return { state: "error", message: `${base}/features returned HTTP ${res.status}.` };
  }
  const features = (await res.json().catch(() => [])).map((f) => f.id);
  return {
    state: "ok",
    features,
    message: `Running on ${base} (features: ${features.join(", ") || "none"}).`,
  };
}

/** Polls checkHealth until it reports "ok" or `timeoutMs` passes; returns
 * the last result either way. */
async function waitForHealthy(port, secret, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  let result = await checkHealth(port, secret);
  while (result.state !== "ok" && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    result = await checkHealth(port, secret);
  }
  return result;
}

/** Whether the Chrome extension has said hello to the running service
 * (see POST /extension/hello in server.ts). Returns { connected, version };
 * any failure counts as not connected. */
async function checkExtensionConnected(port, secret) {
  try {
    const res = await fetch(`http://127.0.0.1:${port}/extension/hello`, {
      headers: { "X-Companion-Secret": secret },
      signal: AbortSignal.timeout(2000),
    });
    if (!res.ok) return { connected: false, version: null };
    const body = await res.json();
    return { connected: body.connected === true, version: body.version || null };
  } catch {
    return { connected: false, version: null };
  }
}

/** The last `lines` non-empty lines of a log file, or "" if it doesn't exist. */
function tailFile(filePath, lines = 15) {
  try {
    return fs.readFileSync(filePath, "utf8").split("\n").filter(Boolean).slice(-lines).join("\n");
  } catch {
    return "";
  }
}

module.exports = {
  COMPANION_SERVICE_DIR,
  buildServer,
  checkHealth,
  checkExtensionConnected,
  waitForHealthy,
  tailFile,
};
