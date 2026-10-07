const os = require("os");
const path = require("path");
const test = require("node:test");
const assert = require("node:assert/strict");
const { sessionCwdAllowed } = require("./session-cwd.js");

const WT = path.join(os.homedir(), ".ai-dev-companion", "sample-app.worktrees");
const opts = { repoPaths: ["/Users/me/gitviews/sample-app"], sessionsRoot: "/Users/me/.ai-dev-companion/sessions" };

test("a mapped clone, a sessions folder and a <repo>.worktrees/<KEY> folder are allowed", () => {
  assert.equal(sessionCwdAllowed("/Users/me/gitviews/sample-app", opts), true);
  assert.equal(sessionCwdAllowed("/Users/me/.ai-dev-companion/sessions/PROJ-7", opts), true);
  assert.equal(sessionCwdAllowed(`${WT}/PROJ-7`, opts), true);
});

test("anything else — the .worktrees folder itself, deeper, traversal, another repo, relative — is refused", () => {
  assert.equal(sessionCwdAllowed(WT, opts), false);
  assert.equal(sessionCwdAllowed(`${WT}/PROJ-7/src`, opts), false);
  assert.equal(sessionCwdAllowed(`${WT}/../evil`, opts), false);
  assert.equal(sessionCwdAllowed(path.join(os.homedir(), ".ai-dev-companion", "other.worktrees", "PROJ-7"), opts), false);
  assert.equal(sessionCwdAllowed("/tmp/evil", opts), false);
  assert.equal(sessionCwdAllowed("sample-app.worktrees/PROJ-7", opts), false);
  assert.equal(sessionCwdAllowed(undefined, opts), false);
});

test("a read-only checkout is allowed exactly two levels under the checkouts root", () => {
  const WT = path.join(os.homedir(), ".ai-dev-companion", "sample-app.worktrees");
const opts = { repoPaths: [], sessionsRoot: "/Users/me/.ai-dev-companion/sessions", checkoutsRoot: "/Users/me/.ai-dev-companion/repos" };
  assert.equal(sessionCwdAllowed("/Users/me/.ai-dev-companion/repos/ACME/sample-app", opts), true);
  assert.equal(sessionCwdAllowed("/Users/me/.ai-dev-companion/repos/ACME", opts), false);
  assert.equal(sessionCwdAllowed("/Users/me/.ai-dev-companion/repos/ACME/sample-app/src", opts), false);
  assert.equal(sessionCwdAllowed("/Users/me/.ai-dev-companion/repos", opts), false);
  assert.equal(sessionCwdAllowed("/Users/me/.ai-dev-companion/repos/ACME/sample-app", { ...opts, checkoutsRoot: undefined }), false);
});
