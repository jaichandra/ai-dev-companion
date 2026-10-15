// The registry of work units: one record per branch the tool has worked on
// (repo + branch), with the worktree it used, the ticket and PR it belongs to
// and every Claude session started there. It is what lets a panel or
// `companion resume` find the session again, and what a later cleanup command
// lists. Operational state, not history: it does not depend on the history
// opt-in and is never expired by its retention. One small JSON file in the
// state dir, written atomically. Plain JS so node:test covers it from source.
const fs = require("fs");
const path = require("path");
const paths = require("./paths.js");

const VERSION = 1;
const MAX_UNITS = 500;
const MAX_SESSIONS = 20;
const SESSION_ID_RE = /^[A-Za-z0-9_-]{1,100}$/;
const STR_MAX = 500;

function str(v) {
  if (typeof v !== "string") return null;
  const s = v.replace(/[\u0000-\u001f\u007f]/g, " ").trim();
  return s ? s.slice(0, STR_MAX) : null;
}

/** `<repo path>#<branch>` — the same branch in two clones is two units. */
function unitKey(repoPath, branch) {
  if (!str(repoPath) || !str(branch)) throw new Error("A work unit needs a repo path and a branch.");
  return `${path.resolve(repoPath)}#${branch}`;
}

function defaultFile(home) {
  return path.join(paths.stateDir(home), "work-units.json");
}

function load(file) {
  try {
    const parsed = JSON.parse(fs.readFileSync(file, "utf8"));
    if (parsed && parsed.version === VERSION && parsed.units && typeof parsed.units === "object") return parsed.units;
  } catch {
    // Missing or unreadable: start empty rather than block the feature.
  }
  return {};
}

function save(file, units) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ version: VERSION, units }, null, 2), { mode: 0o600 });
  fs.renameSync(tmp, file);
}

/**
 * `openWorkUnits({ file, now })` -> the registry. Each call reads the file
 * afresh, so two features (or the service and the CLI) see each other's writes.
 *   upsert({ repoPath, branch, dir?, issueKey?, prKey? }) -> unit
 *       creates the unit or updates the fields given (never blanks one).
 *   addSession(key, { id, cwd, feature }) -> unit | null
 *       records a Claude session; the same id again only refreshes it.
 *   get(key), findByIssue(issueKey), findByPr(prKey), list()
 *   remove(key)
 *   prune(isLive) -> removed units; isLive(unit) says whether its worktree
 *       (or, with no dir, its branch) still exists.
 */
function openWorkUnits({ file = defaultFile(), now = Date.now } = {}) {
  function mutate(fn) {
    const units = load(file);
    const result = fn(units);
    const keys = Object.keys(units);
    if (keys.length > MAX_UNITS) {
      keys
        .sort((a, b) => units[a].updatedAt - units[b].updatedAt)
        .slice(0, keys.length - MAX_UNITS)
        .forEach((k) => delete units[k]);
    }
    save(file, units);
    return result;
  }

  return {
    upsert({ repoPath, branch, dir = null, issueKey = null, prKey = null }) {
      const key = unitKey(repoPath, branch);
      return mutate((units) => {
        const at = now();
        const unit = units[key] || {
          key,
          repoPath: path.resolve(repoPath),
          branch,
          dir: null,
          issueKey: null,
          prKey: null,
          createdAt: at,
          updatedAt: at,
          sessions: [],
        };
        if (str(dir)) unit.dir = str(dir);
        if (str(issueKey)) unit.issueKey = str(issueKey);
        if (str(prKey)) unit.prKey = str(prKey);
        unit.updatedAt = at;
        units[key] = unit;
        return unit;
      });
    },

    addSession(key, { id, cwd = null, feature = null }) {
      if (typeof id !== "string" || !SESSION_ID_RE.test(id)) throw new Error("Not a Claude session id.");
      return mutate((units) => {
        const unit = units[key];
        if (!unit) return null;
        const at = now();
        unit.sessions = unit.sessions.filter((s) => s.id !== id);
        unit.sessions.push({ id, cwd: str(cwd), feature: str(feature), at });
        if (unit.sessions.length > MAX_SESSIONS) unit.sessions = unit.sessions.slice(-MAX_SESSIONS);
        unit.updatedAt = at;
        return unit;
      });
    },

    get: (key) => load(file)[key] || null,
    findByIssue: (issueKey) => Object.values(load(file)).filter((u) => u.issueKey === issueKey),
    findByPr: (prKey) => Object.values(load(file)).filter((u) => u.prKey === prKey),
    list: () => Object.values(load(file)).sort((a, b) => b.updatedAt - a.updatedAt),

    remove: (key) =>
      mutate((units) => {
        const had = key in units;
        delete units[key];
        return had;
      }),

    prune(isLive) {
      return mutate((units) => {
        const removed = [];
        for (const [key, unit] of Object.entries(units)) {
          if (isLive(unit)) continue;
          removed.push(unit);
          delete units[key];
        }
        return removed;
      });
    },
  };
}

module.exports = { openWorkUnits, unitKey, defaultFile, MAX_SESSIONS, MAX_UNITS };
