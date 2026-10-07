const test = require("node:test");
const assert = require("node:assert/strict");
const { MAX_SUBTASK_ROWS, parseIssueKey, assertSubtaskRows } = require("./plan.js");

test("parseIssueKey upper-cases and splits the project key", () => {
  const result = parseIssueKey("proj-34541");
  assert.equal(result.key, "PROJ-34541");
  assert.equal(result.projectKey, "PROJ");
});

test("parseIssueKey accepts a project key with digits/underscores", () => {
  const result = parseIssueKey("CIS2_X-71290");
  assert.equal(result.key, "CIS2_X-71290");
  assert.equal(result.projectKey, "CIS2_X");
});

test("parseIssueKey rejects malformed input", () => {
  assert.throws(() => parseIssueKey("not-an-issue-key"), /doesn't look like a Jira issue key/);
  assert.throws(() => parseIssueKey(""), /doesn't look like a Jira issue key/);
  assert.throws(() => parseIssueKey("PROJ-"), /doesn't look like a Jira issue key/);
});

test("assertSubtaskRows accepts a single well-formed row", () => {
  const result = assertSubtaskRows({ subtasks: [{ summary: "Implement", assignee: "jlangoju" }] });
  assert.deepEqual(result, [{ summary: "Implement", assignee: "jlangoju" }]);
});

test("assertSubtaskRows accepts exactly the max row count", () => {
  const rows = Array.from({ length: MAX_SUBTASK_ROWS }, (_, i) => ({
    summary: `Task ${i}`,
    assignee: "jlangoju",
  }));
  const result = assertSubtaskRows({ subtasks: rows });
  assert.equal(result.length, MAX_SUBTASK_ROWS);
});

test("assertSubtaskRows trims whitespace", () => {
  const result = assertSubtaskRows({ subtasks: [{ summary: "  Implement  ", assignee: "  jlangoju  " }] });
  assert.deepEqual(result, [{ summary: "Implement", assignee: "jlangoju" }]);
});

test("assertSubtaskRows rejects zero rows", () => {
  assert.throws(() => assertSubtaskRows({ subtasks: [] }), /At least one subtask is required/);
});

test("assertSubtaskRows rejects a missing subtasks array", () => {
  assert.throws(() => assertSubtaskRows({}), /At least one subtask is required/);
});

test("assertSubtaskRows rejects more than the max row count", () => {
  const rows = Array.from({ length: MAX_SUBTASK_ROWS + 1 }, (_, i) => ({
    summary: `Task ${i}`,
    assignee: "jlangoju",
  }));
  assert.throws(() => assertSubtaskRows({ subtasks: rows }), /At most 10 subtasks/);
});

test("assertSubtaskRows rejects a blank summary", () => {
  assert.throws(
    () => assertSubtaskRows({ subtasks: [{ summary: "  ", assignee: "jlangoju" }] }),
    /Subtask #1 needs a name/,
  );
});

test("assertSubtaskRows rejects a blank assignee", () => {
  assert.throws(
    () => assertSubtaskRows({ subtasks: [{ summary: "Implement", assignee: "" }] }),
    /Subtask #1 \("Implement"\) needs an assignee/,
  );
});
