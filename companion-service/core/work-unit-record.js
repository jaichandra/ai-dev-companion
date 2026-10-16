// Bookkeeping for the work-unit registry (core/work-units.js) that features
// call as they work on a branch. Never throws: losing a record must not fail a
// job. Plain JS so node:test covers it; features reach it through require.
const { openWorkUnits, unitKey } = require("./work-units.js");

/** Notes that `feature` is working on `branch` of `repoPath` in `dir`. Returns the unit key, or null. */
function recordWorkUnit({ repoPath, branch, dir, issueKey, prKey }, { registry = openWorkUnits } = {}) {
  try {
    return registry().upsert({ repoPath, branch, dir, issueKey, prKey }).key;
  } catch {
    return null;
  }
}

/** Notes a Claude session started for the unit; the unit is made if it isn't there yet. */
function recordWorkUnitSession({ repoPath, branch, dir, issueKey, prKey, session }, { registry = openWorkUnits } = {}) {
  try {
    const reg = registry();
    const unit = reg.upsert({ repoPath, branch, dir, issueKey, prKey });
    reg.addSession(unit.key, session);
    return unit.key;
  } catch {
    return null;
  }
}

/** The stored PR key for the profile's git host (`bitbucket:PROJ/repo#4`, `github:owner/repo#4`), or null. */
function prKeyFor(project, repo, prId) {
  try {
    return require("./prereqs.js").gitRemoteRules().prKey(project, repo, prId);
  } catch {
    return null;
  }
}

module.exports = { prKeyFor, recordWorkUnit, recordWorkUnitSession, unitKey };
