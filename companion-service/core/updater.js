// Versioning and self-update, shared by server.ts (GET /update, POST
// /update/apply), core/update-runner.js (the detached process that applies
// an update) and install.js (which records where this copy came from).
// Plain JS for the same reason as core/prereqs.js: tests and the runner use
// it without a build.
//
// The install source is a git remote + branch (install-source.json, written
// by install.js). Checking keeps a shallow clone of it under
// ~/.ai-dev-companion (core/paths.js's stateDir()) and reads the
// version from its package.json, so it uses the user's existing git
// credentials and needs no browser session.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFile, spawn } = require("child_process");
const paths = require("./paths.js");

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

/** companion-service/ — found by walking up, since the service loads this
 * file from its compiled copy in dist/core/ and setup/tests from core/. */
function findCompanionServiceDir(start = __dirname) {
  for (let dir = start, i = 0; i < 4; i++, dir = path.dirname(dir)) {
    // tsc copies package.json into dist/ (when something requires it), and
    // the built service runs from there: that copy is not the service dir.
    if (path.basename(dir) === "dist") continue;
    if (readJson(path.join(dir, "package.json"))?.name === "bitbucket-ai-companion") return dir;
  }
  return path.join(__dirname, "..");
}

const COMPANION_SERVICE_DIR = findCompanionServiceDir();
const INSTALL_DIR = path.join(COMPANION_SERVICE_DIR, "..");
const INSTALL_SOURCE_PATH = path.join(COMPANION_SERVICE_DIR, "install-source.json");
// Installs follow the profile's ref, not master: pushing to master ships nothing
// until `release` is moved (see docs/DESIGN.md, "Publishing a release").
const DEFAULT_SOURCE = { ...require("../environment.js").updateSource };

/** Delegates to core/paths.js — the single source of the state dir — so
 * every helper below keeps working unchanged after the move to
 * ~/.ai-dev-companion (see core/paths.js's migrateStateDir). `home` is
 * injectable (mirroring paths.js's own functions), defaulting to
 * os.homedir(), so tests can point it at a temp dir instead of stubbing
 * globals. */
function stateDir(home = os.homedir()) {
  return paths.stateDir(home);
}
function sourceCacheDir() {
  return path.join(stateDir(), "update-source");
}
/** A separate clone for applying an update, so the periodic check (which
 * syncs sourceCacheDir) can never rewrite it mid-install. */
function stagingDir() {
  return path.join(stateDir(), "update-staging");
}

/** `preferred` if it exists, else `fallback` if THAT exists, else
 * `preferred` anyway (so a first write still lands in the new dir). */
function existingOr(preferred, fallback) {
  if (fs.existsSync(preferred)) return preferred;
  return fs.existsSync(fallback) ? fallback : preferred;
}

/** update-status.json and update.log fall back to the legacy state dir
 * when the new one doesn't have them yet, for READS only: an
 * update-runner.js started before a migration may have left its status
 * file in the old location. `home` is injectable for the same reason as
 * stateDir() above.
 *
 * Writes (writeStatus's default, and the log fd startUpdate opens) always
 * go to newStatusPath()/newLogPath() below instead — never here. This
 * function being sticky for writes too was a bug: once a legacy file
 * existed, `existingOr` would keep pointing every future write back at
 * the legacy dir forever (it never re-checks whether the new dir now has
 * its own file), so a service that had ever run pre-migration would go on
 * writing update-status.json/update.log into `~/.bitbucket-ai-companion`
 * indefinitely, even long after the migration moved everything else. */
function statusPath(home = os.homedir()) {
  return existingOr(
    path.join(stateDir(home), "update-status.json"),
    path.join(paths.legacyStateDir(home), "update-status.json"),
  );
}
function logPath(home = os.homedir()) {
  return existingOr(
    path.join(stateDir(home), "update.log"),
    path.join(paths.legacyStateDir(home), "update.log"),
  );
}

/** Where update-status.json/update.log are always WRITTEN — the current
 * state dir, never the legacy one, regardless of which file existingOr
 * above would currently read. See statusPath()/logPath()'s comment. */
function newStatusPath(home = os.homedir()) {
  return path.join(stateDir(home), "update-status.json");
}
function newLogPath(home = os.homedir()) {
  return path.join(stateDir(home), "update.log");
}
/** Where install.js always installs to; only a copy running from here can
 * update itself (a dev checkout run with `npm start` can't). */
function stableInstallDir() {
  return path.join(os.homedir(), "ai-dev-companion");
}

/** "0.10.2" > "0.9.9". Missing parts count as 0; anything after "-" or "+"
 * is ignored. Returns -1, 0 or 1. */
function compareVersions(a, b) {
  const parse = (v) =>
    String(v || "")
      .trim()
      .replace(/^v/, "")
      .split(/[-+]/)[0]
      .split(".")
      .map((n) => parseInt(n, 10) || 0);
  const pa = parse(a);
  const pb = parse(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d !== 0) return d > 0 ? 1 : -1;
  }
  return 0;
}

/** Markdown inline formatting reduced to plain text: the extension renders
 * notes with textContent. */
function plainText(markdown) {
  return markdown
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/\*\*|__|`/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

/**
 * CHANGELOG.md's release sections as [{ version, notes }] in file order.
 * A section starts at "## <version>" (anything after the version is
 * ignored); its notes are its "- " bullets, with indented continuation
 * lines joined on. Other sections ("Unreleased") and text are skipped.
 */
function parseChangelog(text) {
  const entries = [];
  let entry = null;
  let note = null;
  for (const line of String(text || "").split(/\r?\n/)) {
    const heading = line.match(/^##\s+v?(\d+(?:\.\d+)*)\b/);
    if (heading || /^#{1,2}\s/.test(line)) {
      entry = heading ? { version: heading[1], notes: [] } : null;
      if (entry) entries.push(entry);
      note = null;
    } else if (!entry) {
      continue;
    } else if (/^[-*]\s+/.test(line)) {
      note = line.replace(/^[-*]\s+/, "");
      entry.notes.push(note);
    } else if (note !== null && /^\s+\S/.test(line)) {
      entry.notes[entry.notes.length - 1] = note = `${note} ${line.trim()}`;
    } else {
      note = null;
    }
  }
  return entries
    .map((e) => ({ version: e.version, notes: e.notes.map(plainText).filter(Boolean) }))
    .filter((e) => e.notes.length > 0);
}

function readChangelog(dir) {
  try {
    return parseChangelog(fs.readFileSync(path.join(dir, "CHANGELOG.md"), "utf8"));
  } catch {
    return [];
  }
}

/** The entries newer than `current` and no newer than `latest`, newest first. */
function whatsNewSince(entries, current, latest) {
  return (entries || [])
    .filter(
      (e) => compareVersions(e.version, current) > 0 && (!latest || compareVersions(e.version, latest) <= 0),
    )
    .sort((a, b) => compareVersions(b.version, a.version));
}

function readPackageVersion(dir) {
  const pkg = readJson(path.join(dir, "package.json"));
  return pkg && typeof pkg.version === "string" ? pkg.version : null;
}

/** This copy's version — the root package.json, which a test keeps in
 * lockstep with companion-service/package.json and the extension manifest. */
function installedVersion(installDir = INSTALL_DIR) {
  return readPackageVersion(installDir) || readPackageVersion(path.join(installDir, "companion-service"));
}

/** { url, ref } from install-source.json, falling back to the default repo. */
function parseInstallSource(raw) {
  if (!raw || typeof raw !== "object") return { ...DEFAULT_SOURCE };
  const url = typeof raw.url === "string" && raw.url.trim() ? raw.url.trim() : DEFAULT_SOURCE.url;
  const ref =
    typeof raw.ref === "string" && /^[A-Za-z0-9._/-]+$/.test(raw.ref) ? raw.ref : DEFAULT_SOURCE.ref;
  return { url, ref };
}

/**
 * Where a copy installed from `sourceDir` should take its updates from: the
 * root package.json's `updateSource` if it declares one, else the clone's
 * git origin, else the default repo. `updateSource` is how the repo moves
 * without anyone reinstalling: publish a release declaring the new location
 * from the old repo, and every install switches to it when it updates (the
 * update runs the *new* release's install.js, which calls this).
 * `url`/`ref` override both (AI_DEV_COMPANION_UPDATE_URL/_REF, for
 * testing against a local repo).
 */
function resolveInstallSource({ sourceDir, origin, url, ref } = {}) {
  const declared = sourceDir ? readJson(path.join(sourceDir, "package.json"))?.updateSource : null;
  const base = declared?.url
    ? parseInstallSource(declared)
    : parseInstallSource(origin ? { url: origin } : null);
  return parseInstallSource({ url: url || base.url, ref: ref || base.ref });
}

function readInstallSource(file = INSTALL_SOURCE_PATH) {
  return parseInstallSource(readJson(file));
}

/** What to run by hand if updating from the browser isn't possible. */
function manualUpdateCommand(source) {
  if (/^(https?|ssh):\/\//.test(source.url) || /^[^/]+@[^:]+:/.test(source.url)) {
    const gitUrl = source.url.startsWith("git@") ? `ssh://${source.url.replace(":", "/")}` : source.url;
    return `npx --yes "git+${gitUrl}#${source.ref}"`;
  }
  return `cd "${source.url}" && git pull && node install.js`;
}

function gitEnv() {
  return {
    ...process.env,
    GIT_TERMINAL_PROMPT: "0",
    GIT_SSH_COMMAND: process.env.GIT_SSH_COMMAND || "ssh -o BatchMode=yes",
  };
}

function runGit(args, cwd) {
  return new Promise((resolve, reject) => {
    execFile(
      "git",
      args,
      { cwd, env: gitEnv(), timeout: 120000, encoding: "utf8" },
      (err, stdout, stderr) => {
        if (err) reject(new Error((stderr || err.message).trim().split("\n").slice(-3).join("\n")));
        else resolve(stdout.trim());
      },
    );
  });
}

/** Brings the shallow clone of the source up to date (cloning it the first
 * time, or again if the source URL changed) and returns its directory. */
async function syncSource(source, dir = sourceCacheDir()) {
  let reuse = fs.existsSync(path.join(dir, ".git"));
  if (reuse) {
    const origin = await runGit(["remote", "get-url", "origin"], dir).catch(() => "");
    reuse = origin === source.url;
  }
  if (reuse) {
    await runGit(["fetch", "--depth", "1", "--quiet", "origin", source.ref], dir);
    await runGit(["reset", "--hard", "--quiet", "FETCH_HEAD"], dir);
    await runGit(["clean", "-fdxq"], dir);
  } else {
    fs.rmSync(dir, { recursive: true, force: true });
    fs.mkdirSync(path.dirname(dir), { recursive: true });
    await runGit(
      ["clone", "--depth", "1", "--quiet", "--branch", source.ref, source.url, dir],
      path.dirname(dir),
    );
  }
  return dir;
}

let lastCheck = null;
let checkInFlight = null;

/** Latest version at the install source, cached for `maxAgeMs`. Never
 * throws: a failure is returned as `error` (e.g. offline, no credentials). */
async function checkForUpdate({ maxAgeMs = 6 * 60 * 60 * 1000, source = readInstallSource() } = {}) {
  if (lastCheck && Date.now() - lastCheck.checkedAt < maxAgeMs) return lastCheck;
  if (!checkInFlight) {
    checkInFlight = (async () => {
      try {
        const dir = await syncSource(source);
        const latest = installedVersion(dir);
        if (!latest) throw new Error(`${source.url} (${source.ref}) has no package.json version.`);
        lastCheck = { latest, changelog: readChangelog(dir), error: null, checkedAt: Date.now() };
      } catch (err) {
        lastCheck = {
          latest: lastCheck?.latest || null,
          changelog: lastCheck?.changelog || [],
          error: err.message,
          checkedAt: Date.now(),
        };
      } finally {
        checkInFlight = null;
      }
      return lastCheck;
    })();
  }
  return checkInFlight;
}

function isAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** The last (or current) update attempt from update-status.json, or null.
 * A "running" attempt whose runner process is gone is reported as failed. */
function readStatus(file = statusPath()) {
  const status = readJson(file);
  if (!status || typeof status !== "object") return null;
  if (status.state === "running" && status.pid && !isAlive(status.pid)) {
    return {
      ...status,
      state: "failed",
      error: `The updater stopped unexpectedly. See ${logPath()} for details.`,
    };
  }
  return status;
}

function writeStatus(status, file = newStatusPath()) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(status, null, 2) + "\n");
  fs.renameSync(tmp, file);
}

/** Why this copy can't update itself from the browser, or null if it can. */
function cannotApplyReason(installDir = INSTALL_DIR) {
  const real = (p) => {
    try {
      return fs.realpathSync(p);
    } catch {
      return path.resolve(p);
    }
  };
  if (real(installDir) !== real(stableInstallDir())) {
    return `This service runs from ${installDir}, not the installed copy in ${stableInstallDir()}.`;
  }
  return null;
}

/**
 * Everything the extension needs to decide whether to offer an update.
 * `underLaunchd` means the service can be restarted by just exiting (the
 * launchd job's KeepAlive starts it again); otherwise the user has to.
 */
async function getUpdateInfo({ refresh = false, underLaunchd = false, runningVersion = null } = {}) {
  const source = readInstallSource();
  // The version this process started with — not what's on disk, which an
  // update replaces before the service restarts.
  const current = runningVersion || installedVersion();
  // Without `refresh`, never wait on git: answer from the last check and let
  // a stale one refresh in the background (the extension asks every minute).
  let check;
  if (refresh) {
    check = await checkForUpdate({ maxAgeMs: 0, source });
  } else {
    const pending = checkForUpdate({ source });
    check = lastCheck || { latest: null, error: null, checkedAt: null };
    void pending;
  }
  const reason = cannotApplyReason();
  return {
    current,
    latest: check.latest,
    available: !!(check.latest && current && compareVersions(check.latest, current) > 0),
    whatsNew: current ? whatsNewSince(check.changelog, current, check.latest) : [],
    checkedAt: check.checkedAt,
    error: check.error,
    source,
    canApply: !reason,
    cannotApplyReason: reason,
    restartsItself: underLaunchd,
    manualCommand: manualUpdateCommand(source),
    update: readStatus(),
  };
}

/**
 * Starts core/update-runner.js as a detached process (so it outlives this
 * service, which it restarts at the end) and returns the initial status.
 * `servicePid` is only passed when the service runs under launchd, so the
 * runner restarts it by stopping it; otherwise the user restarts it.
 */
function startUpdate({ servicePid = null } = {}) {
  const existing = readStatus();
  if (existing && existing.state === "running") return existing;
  const reason = cannotApplyReason();
  if (reason) throw new Error(reason);

  // Written before spawning, so the runner's own (later) writes always win.
  const status = {
    state: "running",
    step: "download",
    label: "Downloading the latest version…",
    from: installedVersion(),
    to: null,
    pid: null,
    startedAt: Date.now(),
  };
  writeStatus(status);
  const out = fs.openSync(newLogPath(), "w");
  const child = spawn(
    process.execPath,
    [
      path.join(COMPANION_SERVICE_DIR, "core", "update-runner.js"),
      ...(servicePid ? ["--restart-pid", String(servicePid)] : []),
    ],
    { cwd: stateDir(), detached: true, stdio: ["ignore", out, out], env: process.env },
  );
  child.unref();
  return { ...status, pid: child.pid };
}

module.exports = {
  DEFAULT_SOURCE,
  resolveInstallSource,
  INSTALL_SOURCE_PATH,
  compareVersions,
  installedVersion,
  parseChangelog,
  readChangelog,
  whatsNewSince,
  parseInstallSource,
  readInstallSource,
  manualUpdateCommand,
  syncSource,
  checkForUpdate,
  readStatus,
  writeStatus,
  cannotApplyReason,
  getUpdateInfo,
  startUpdate,
  sourceCacheDir,
  stagingDir,
  statusPath,
  logPath,
  newStatusPath,
  newLogPath,
  stableInstallDir,
};
