const test = require("node:test");
const assert = require("node:assert/strict");
const resume = require("./resume.js");

const SESSION_ID = "11111111-2222-3333-4444-555555555555";

test("claudeProjectSlug replaces every non-alphanumeric character with -", () => {
  // The verified example from global-constraints.md.
  assert.equal(
    resume.claudeProjectSlug("/Users/jail/.bitbucket-ai-companion/worktrees/x"),
    "-Users-jail--bitbucket-ai-companion-worktrees-x",
  );
});

test("sessionTranscriptPath joins home, .claude/projects, the cwd's slug and <id>.jsonl", () => {
  assert.equal(
    resume.sessionTranscriptPath("/Users/jail", "/Users/jail/work/repo", SESSION_ID),
    `/Users/jail/.claude/projects/-Users-jail-work-repo/${SESSION_ID}.jsonl`,
  );
});

test("realCwdForTranscript resolves cwd through the injected realpath implementation", () => {
  const calls = [];
  const realpathImpl = (p) => {
    calls.push(p);
    return "/Users/jail/real/work/repo";
  };
  assert.equal(
    resume.realCwdForTranscript("/Users/jail/work/repo", realpathImpl),
    "/Users/jail/real/work/repo",
  );
  assert.deepEqual(calls, ["/Users/jail/work/repo"]);
});

test("realCwdForTranscript falls back to the unresolved cwd when realpath throws", () => {
  const realpathImpl = () => {
    throw new Error("ENOENT: no such file or directory");
  };
  assert.equal(resume.realCwdForTranscript("/Users/jail/work/gone", realpathImpl), "/Users/jail/work/gone");
});

test("buildResumeArgs returns --resume/id/--permission-mode/mode", () => {
  const session = { id: SESSION_ID, cwd: "/x", permissionMode: "plan" };
  assert.deepEqual(resume.buildResumeArgs({ session }), [
    "--resume",
    SESSION_ID,
    "--permission-mode",
    "plan",
  ]);
});

test("validateSession accepts a UUID id, absolute cwd and a known permission mode", () => {
  assert.equal(
    resume.validateSession({ id: SESSION_ID, cwd: "/Users/jail/work", permissionMode: "plan" }),
    true,
  );
  assert.equal(
    resume.validateSession({ id: SESSION_ID, cwd: "/Users/jail/work", permissionMode: "default" }),
    true,
  );
});

test("validateSession rejects a non-UUID id, a relative cwd, an unknown permission mode, or a missing session", () => {
  const base = { id: SESSION_ID, cwd: "/Users/jail/work", permissionMode: "plan" };
  assert.equal(resume.validateSession({ ...base, id: "not-a-uuid" }), false);
  assert.equal(resume.validateSession({ ...base, id: undefined }), false);
  assert.equal(resume.validateSession({ ...base, cwd: "relative/path" }), false);
  assert.equal(resume.validateSession({ ...base, cwd: undefined }), false);
  assert.equal(resume.validateSession({ ...base, permissionMode: "bypassPermissions" }), false);
  assert.equal(resume.validateSession({ ...base, permissionMode: undefined }), false);
  assert.equal(resume.validateSession(undefined), false);
  assert.equal(resume.validateSession(null), false);
});
