// resumeSessionInTerminal from the build (npm run build first). Only the
// refusals are exercised — they return before any terminal is opened — with
// HOME pointed at a temp dir so no real transcript folder is ever looked at.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "resume-session-home-"));
const { resumeSessionInTerminal } = require("../dist/core/resume-session.js");

const { repoWorktreesRoot } = require("./paths.js");
const ID = "3f2b8c1e-1111-4222-8333-444455556666";

test("a session this service wouldn't have recorded is refused as invalid", async () => {
  for (const bad of [null, {}, { id: "x", cwd: "/tmp", permissionMode: "plan" }, { id: ID, cwd: "relative", permissionMode: "plan" }, { id: ID, cwd: "/tmp", permissionMode: "bypassPermissions" }]) {
    assert.deepEqual(await resumeSessionInTerminal({}, bad), { ok: false, reason: "invalid" });
  }
});

test("a session whose folder or transcript is gone is refused as gone", async () => {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "resume-session-cwd-"));
  try {
    // The folder exists but ~/.claude/projects/<slug>/<id>.jsonl doesn't.
    const config = { repos: { "ACME/sample-app": cwd } };
    assert.deepEqual(await resumeSessionInTerminal(config, { id: ID, cwd, permissionMode: "plan" }), { ok: false, reason: "gone" });
    // <stateDir>/<repo>.worktrees/<KEY> is allowed but absent here.
    assert.deepEqual(
      await resumeSessionInTerminal(config, { id: ID, cwd: path.join(repoWorktreesRoot(cwd), "PROJ-1"), permissionMode: "plan" }),
      { ok: false, reason: "gone" },
    );
  } finally {
    fs.rmSync(cwd, { recursive: true, force: true });
  }
});

test("a session folder that isn't a mapped repo, a ticket worktree or a sessions folder is refused before anything is looked at", async () => {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "resume-session-evil-"));
  try {
    const session = { id: ID, cwd, permissionMode: "plan" };
    assert.deepEqual(await resumeSessionInTerminal({}, session), { ok: false, reason: "invalid" });
    assert.deepEqual(await resumeSessionInTerminal({ repos: { "ACME/sample-app": "/elsewhere" } }, session), { ok: false, reason: "invalid" });
    // The job's own worktree is the one extra folder a caller may vouch for.
    assert.deepEqual(await resumeSessionInTerminal({}, session, [], cwd), { ok: false, reason: "gone" });
  } finally {
    fs.rmSync(cwd, { recursive: true, force: true });
  }
});
