// macOS-only background-service management via launchd (a per-user
// "LaunchAgent" — no sudo/root needed, unlike a system-level
// LaunchDaemon). Plain JS, not TypeScript — same reason as
// core/prereqs.js: it lets launchd.test.js run this directly with zero
// build step, and setup.js/service.js can both require() it pre-build.
//
// Split deliberately into pure, directly-testable helpers (renderPlist,
// plistPath, logPaths, serverEntryPath) and thin side-effecting wrappers
// (installService/uninstallService/serviceStatus) that shell out to
// launchctl — the latter are not unit-tested for the same reason
// core/worktree.ts's addWorktree/removeWorktree aren't: they're real,
// externally-visible actions (installing a real launchd job on whatever
// machine runs the test), not logic to verify.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

// Overridable so a second copy (e.g. a dev checkout) can run as its own
// job without replacing the installed one.
const LABEL = process.env.AI_DEV_COMPANION_SERVICE_LABEL || "com.ai-dev-companion.companion-service";
const COMPANION_SERVICE_DIR = path.join(__dirname, "..");

function plistPath() {
  return path.join(os.homedir(), "Library", "LaunchAgents", `${LABEL}.plist`);
}

function logPaths() {
  return {
    out: path.join(COMPANION_SERVICE_DIR, "companion-service.out.log"),
    err: path.join(COMPANION_SERVICE_DIR, "companion-service.err.log"),
  };
}

function serverEntryPath() {
  return path.join(COMPANION_SERVICE_DIR, "dist", "server.js");
}

function escapeXml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

/**
 * Pure plist rendering — every path/value that matters is passed in
 * explicitly so this is testable without touching the filesystem or
 * process.env. `nodePath` is deliberately the *absolute* path to the node
 * binary (process.execPath), and `pathEnv` deliberately the *caller's*
 * PATH: launchd runs LaunchAgents with a minimal environment (no
 * ~/.zshrc, no nvm/homebrew shims), so without this, `git`/`claude` — both
 * invoked by bare name via child_process, not shell:true — would resolve
 * against launchd's bare-bones PATH instead of the one this was actually
 * configured under, and silently fail as if git/claude were missing.
 */
function renderPlist({ label, nodePath, serverPath, workingDir, pathEnv, outLog, errLog }) {
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${escapeXml(label)}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${escapeXml(nodePath)}</string>
    <string>${escapeXml(serverPath)}</string>
  </array>
  <key>WorkingDirectory</key>
  <string>${escapeXml(workingDir)}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>${escapeXml(pathEnv)}</string>
    <key>AI_DEV_COMPANION_UNDER_LAUNCHD</key>
    <string>1</string>
    <key>NODE_USE_SYSTEM_CA</key>
    <string>1</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>StandardOutPath</key>
  <string>${escapeXml(outLog)}</string>
  <key>StandardErrorPath</key>
  <string>${escapeXml(errLog)}</string>
</dict>
</plist>
`;
}

const STANDARD_PATH_DIRS = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/**
 * The PATH to bake into the plist: the caller's PATH minus entries that
 * only exist for the duration of an `npm run`/`npx` invocation (npm
 * prepends every ancestor's node_modules/.bin, its node-gyp shim, and npx's
 * throwaway cache dir), plus the standard system dirs if missing — so
 * git/claude still resolve even from a stripped-down shell.
 */
function buildServicePath(pathEnv) {
  const kept = (pathEnv || "")
    .split(":")
    .filter((dir) => dir && !/node_modules[\\/]\.bin|node-gyp-bin|[\\/]_npx[\\/]/.test(dir));
  const unique = [...new Set([...kept, ...STANDARD_PATH_DIRS])];
  return unique.join(":");
}

function guiDomain() {
  return `gui/${process.getuid()}`;
}

function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function isLoaded() {
  try {
    execFileSync("launchctl", ["print", `${guiDomain()}/${LABEL}`], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

/** Unloads whatever's currently loaded under this label and waits for
 * launchd to actually finish — `bootout` returns before the job is gone,
 * and bootstrapping again too soon fails with "5: Input/output error". */
function unloadIfLoaded() {
  if (!isLoaded()) return;
  try {
    execFileSync("launchctl", ["bootout", `${guiDomain()}/${LABEL}`], { stdio: "ignore" });
  } catch {
    // Already on its way out — the wait below covers it either way.
  }
  for (let i = 0; i < 50 && isLoaded(); i++) sleepMs(100);
}

function bootstrapWithRetry(target) {
  let lastError;
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      execFileSync("launchctl", ["bootstrap", guiDomain(), target], { stdio: "pipe" });
      return;
    } catch (err) {
      lastError = err;
      sleepMs(500 * (attempt + 1));
    }
  }
  const detail = lastError.stderr ? String(lastError.stderr).trim() : lastError.message;
  throw new Error(`launchctl could not load ${target}: ${detail}`);
}

/**
 * Writes the plist and (re)loads it under launchd. Requires
 * `npm run build` to have already produced dist/server.js — a background
 * job pointed at a file that doesn't exist yet would just crash-loop
 * silently, so that's checked up front with a clear error instead.
 */
function installService() {
  const serverPath = serverEntryPath();
  if (!fs.existsSync(serverPath)) {
    throw new Error(
      `${serverPath} doesn't exist yet — run "npm run build" first, then re-run this.`,
    );
  }

  const { out, err } = logPaths();
  const plist = renderPlist({
    label: LABEL,
    nodePath: process.execPath,
    serverPath,
    workingDir: COMPANION_SERVICE_DIR,
    pathEnv: buildServicePath(process.env.PATH),
    outLog: out,
    errLog: err,
  });

  const target = plistPath();
  fs.mkdirSync(path.dirname(target), { recursive: true });
  unloadIfLoaded();
  fs.writeFileSync(target, plist);
  bootstrapWithRetry(target);

  return { label: LABEL, plistPath: target, outLog: out, errLog: err };
}

function isInstalled() {
  return fs.existsSync(plistPath());
}

/** Unloads the job and deletes its plist. Returns false if there was
 * nothing installed to remove (idempotent — safe to run more than once,
 * or on a machine that never had it installed). */
function uninstallService() {
  const target = plistPath();
  const existed = fs.existsSync(target);
  unloadIfLoaded();
  if (existed) fs.rmSync(target, { force: true });
  return existed;
}

/** { loaded, pid } — pid is null if the job is loaded but not currently
 * running (e.g. it crashed and launchd hasn't respawned it yet). */
function serviceStatus() {
  let output;
  try {
    // stdio: pipe stdout (need it), ignore stderr — launchctl writes its
    // own "Could not find service" straight to the terminal otherwise,
    // even though this is an entirely expected, already-handled outcome
    // (not installed yet) rather than something worth surfacing raw.
    output = execFileSync("launchctl", ["list", LABEL], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
  } catch {
    return { loaded: false, pid: null };
  }
  // Modern launchctl (confirmed on this machine) prints a plist-like
  // dictionary for `launchctl list <label>`, e.g. `"PID" = 8478;` when
  // running, with no PID key at all when loaded but not currently
  // running. Older documented output is instead a single
  // "PID\tStatus\tLabel" line ("-" for PID when not running) — kept as a
  // fallback in case that's what's ever actually on PATH.
  const dictMatch = output.match(/"PID"\s*=\s*(\d+)\s*;/);
  if (dictMatch) {
    return { loaded: true, pid: dictMatch[1] };
  }
  if (/^\{/.test(output.trim())) {
    return { loaded: true, pid: null }; // dictionary present, just no PID key
  }
  const fields = output.trim().split(/\s+/);
  const pid = fields[0] && fields[0] !== "-" ? fields[0] : null;
  return { loaded: true, pid };
}

module.exports = {
  LABEL,
  plistPath,
  logPaths,
  serverEntryPath,
  renderPlist,
  buildServicePath,
  isInstalled,
  installService,
  uninstallService,
  serviceStatus,
};
