// Pure logic for create-jira-subtasks. Plain JS, not TypeScript — same
// reason as core/prereqs.js: it lets plan.test.js run this directly with
// zero build step; tsc still picks it up (allowJs) and copies it into
// dist/ for index.ts's runtime use after a build.
//
// The feature itself is a generic "add up to 10 subtasks, each with its
// own name and assignee" tool — the extension's form collects the rows
// directly, so there's no fixed ladder or default-assignee mapping to
// encode here anymore (that lives only in the standalone
// ~/.local/bin/create-jira-subtasks script this was originally ported
// from, left untouched for terminal use).
const ISSUE_KEY_RE = /^([A-Z][A-Z0-9_]*)-(\d+)$/;
const MAX_SUBTASK_ROWS = 10;

/**
 * Upper-cases and validates an issue key the same way the script's
 * `tr '[:lower:]' '[:upper:]'` + `${PARENT_ISSUE%%-*}` did, but rejecting
 * malformed input instead of silently producing a bad project key.
 */
function parseIssueKey(raw) {
  const key = String(raw || "").trim().toUpperCase();
  const match = key.match(ISSUE_KEY_RE);
  if (!match) {
    throw new Error(`"${raw}" doesn't look like a Jira issue key (expected e.g. "PROJ-34541").`);
  }
  return { key, projectKey: match[1] };
}

/**
 * Validates the extension panel's submitted rows: `body.subtasks` must be
 * an array of 1-10 entries, each a non-blank { summary, assignee }. This is
 * the server's own belt-and-suspenders check (the form does its own
 * friendlier validation first) — same relationship resolve-conflict's
 * assertPayload has to its client. Returns the validated, trimmed
 * [{summary, assignee}].
 */
function assertSubtaskRows(body) {
  const subtasks = body && Array.isArray(body.subtasks) ? body.subtasks : null;
  if (!subtasks || subtasks.length === 0) {
    throw new Error("At least one subtask is required.");
  }
  if (subtasks.length > MAX_SUBTASK_ROWS) {
    throw new Error(`At most ${MAX_SUBTASK_ROWS} subtasks can be created at once.`);
  }
  return subtasks.map((row, i) => {
    const summary = row && String(row.summary || "").trim();
    const assignee = row && String(row.assignee || "").trim();
    if (!summary) {
      throw new Error(`Subtask #${i + 1} needs a name.`);
    }
    if (!assignee) {
      throw new Error(`Subtask #${i + 1} ("${summary}") needs an assignee.`);
    }
    return { summary, assignee };
  });
}

module.exports = {
  MAX_SUBTASK_ROWS,
  parseIssueKey,
  assertSubtaskRows,
};
