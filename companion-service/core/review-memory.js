// What Review PR remembers about each PR it has reviewed in a Claude Code
// terminal: the session id, the folder it ran in and the head commit it
// reviewed, so a second click can reopen that session (head unchanged) or
// review only the new commits. One small JSON file in the state dir, keyed
// by features/review-in-editor/plan.js's prKey. Plain JS like the other
// core helpers so its test runs with no build step.
const fs = require("fs");
const path = require("path");

const FILE_NAME = "review-memory.json";
const MAX_AGE_MS = 30 * 24 * 60 * 60 * 1000;

function fileIn(dir) {
  return path.join(dir, FILE_NAME);
}

function readAll(dir) {
  try {
    const all = JSON.parse(fs.readFileSync(fileIn(dir), "utf8"));
    return all && typeof all === "object" && !Array.isArray(all) ? all : {};
  } catch {
    return {};
  }
}

/** The remembered review for `key`, or null. */
function readReview(key, dir = require("./paths.js").stateDir()) {
  const record = readAll(dir)[key];
  return record && typeof record === "object" ? record : null;
}

/** Remembers `record` ({sessionId, cwd, reviewedSha, reviewedAt}) for `key`,
 * dropping entries older than 30 days. */
function writeReview(key, record, dir = require("./paths.js").stateDir(), now = Date.now()) {
  const all = readAll(dir);
  for (const [k, v] of Object.entries(all)) {
    if (!v || typeof v.reviewedAt !== "number" || now - v.reviewedAt > MAX_AGE_MS) delete all[k];
  }
  all[key] = record;
  fs.mkdirSync(dir, { recursive: true });
  const tmp = `${fileIn(dir)}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(all, null, 2), { mode: 0o600 });
  fs.renameSync(tmp, fileIn(dir));
}

module.exports = { readReview, writeReview };
