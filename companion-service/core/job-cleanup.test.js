const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const jobFiles = require("./job-files.js");
const { sessionCwdAllowed } = require("./session-cwd.js");
const { removePrunedWorktree } = require("./worktree-prune.js");

const NOW = 2_000_000_000_000;
const DAY = 24 * 60 * 60 * 1000;
const ID = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "job-cleanup-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

const analysisJob = (over = {}) => ({
  id: ID,
  featureId: "analyze-issue",
  status: "awaiting-approval",
  data: { claudeSession: { id: "s1", cwd: "/state/sessions/PROJ-1", permissionMode: "plan" } },
  createdAt: NOW - 1000,
  updatedAt: NOW - 1000,
  ...over,
});


test("isStale: only awaiting-approval and failed jobs, only past the age limit", () => {
  const old = NOW - 15 * DAY;
  assert.equal(jobFiles.isStale({ status: "awaiting-approval", updatedAt: old }, NOW), true);
  assert.equal(jobFiles.isStale({ status: "failed", updatedAt: old }, NOW), true);
  assert.equal(jobFiles.isStale({ status: "running", updatedAt: old }, NOW), false);
  assert.equal(jobFiles.isStale({ status: "awaiting-approval", updatedAt: NOW - DAY }, NOW), false);
  assert.equal(jobFiles.isStale({ status: "failed", updatedAt: "x" }, NOW), false);
});

test("loadJobs still drops a worktree-less job with a Claude session unless sessionCwdOk allows it", (t) => {
  const root = tmp(t);
  const dir = jobFiles.jobsDir(root);
  jobFiles.saveJob(dir, analysisJob());
  const denied = jobFiles.loadJobs(dir, { now: NOW, worktreeRoot: path.join(root, "worktrees") });
  assert.deepEqual(denied.jobs, []);
  assert.deepEqual(denied.dropped, [ID]);

  jobFiles.saveJob(dir, analysisJob());
  const allowed = jobFiles.loadJobs(dir, {
    now: NOW,
    worktreeRoot: path.join(root, "worktrees"),
    sessionCwdOk: (cwd) => cwd === "/state/sessions/PROJ-1",
  });
  assert.deepEqual(allowed.jobs.map((j) => j.id), [ID]);
  assert.deepEqual(allowed.dropped, []);
});

test("loadJobs keeps requiring a worktree job's session cwd to be its own worktree", (t) => {
  const root = tmp(t);
  const worktreeRoot = path.join(root, "worktrees");
  const dir = jobFiles.jobsDir(root);
  const wt = path.join(worktreeRoot, "sample-app", ID);
  const job = {
    id: ID,
    featureId: "resolve-conflict",
    status: "awaiting-approval",
    data: {
      worktree: { id: ID, dir: wt, repoPath: "/repo" },
      claudeSession: { id: "s1", cwd: "/somewhere/else", permissionMode: "default" },
    },
    createdAt: NOW - 1000,
    updatedAt: NOW - 1000,
  };
  jobFiles.saveJob(dir, job);
  // Even a permissive predicate must not rescue a worktree job whose session cwd isn't the worktree.
  const r = jobFiles.loadJobs(dir, { now: NOW, worktreeRoot, sessionCwdOk: () => true });
  assert.deepEqual(r.dropped, [ID]);
});

test("loadJobs hands back the pruned job objects, worktree data included", (t) => {
  const root = tmp(t);
  const worktreeRoot = path.join(root, "worktrees");
  const dir = jobFiles.jobsDir(root);
  const wt = path.join(worktreeRoot, "sample-app", ID);
  jobFiles.saveJob(dir, {
    id: ID,
    featureId: "address-review-comments",
    status: "awaiting-approval",
    data: { worktree: { id: ID, dir: wt, repoPath: "/repo" }, claudeSession: { id: "s1", cwd: wt, permissionMode: "default" } },
    createdAt: NOW - 30 * DAY,
    updatedAt: NOW - 30 * DAY,
  });
  const r = jobFiles.loadJobs(dir, { now: NOW, worktreeRoot });
  assert.deepEqual(r.pruned, [ID]);
  assert.equal(r.prunedJobs.length, 1);
  assert.equal(r.prunedJobs[0].id, ID);
  assert.equal(r.prunedJobs[0].data.worktree.dir, wt);
  assert.deepEqual(r.jobs, []);
});

test("sessionCwdAllowed: a mapped repo clone or a directory under the sessions root, nothing else", () => {
  const opts = { repoPaths: ["/home/me/gitviews/sample-app", "/home/me/gitviews/billing"], sessionsRoot: "/state/sessions" };
  assert.equal(sessionCwdAllowed("/home/me/gitviews/sample-app", opts), true);
  assert.equal(sessionCwdAllowed("/state/sessions/PROJ-1", opts), true);
  assert.equal(sessionCwdAllowed("/state/sessions", opts), false);
  assert.equal(sessionCwdAllowed("/state/sessions/../evil", opts), false);
  assert.equal(sessionCwdAllowed("/tmp/attacker", opts), false);
  assert.equal(sessionCwdAllowed("relative/dir", opts), false);
  assert.equal(sessionCwdAllowed(undefined, opts), false);
  assert.equal(sessionCwdAllowed("/home/me/gitviews/sample-app/sub", opts), false);
});

test("removePrunedWorktree removes through git, falls back to a plain delete when git metadata is compromised, and refuses paths outside the root", async () => {
  const worktreeRoot = "/state/worktrees";
  const calls = [];
  const deps = {
    worktreeRoot,
    removeWorktree: async (h) => calls.push(["git", h.dir]),
    rmSync: (dir, opts) => calls.push(["rm", dir, opts]),
  };
  const handle = { id: ID, dir: "/state/worktrees/sample-app/" + ID, repoPath: "/repo" };

  assert.equal(await removePrunedWorktree({ data: {} }, deps), "none");
  assert.equal(await removePrunedWorktree({ data: { worktree: handle } }, deps), "removed");
  assert.equal(
    await removePrunedWorktree({ data: { worktree: handle, gitMetadataCompromised: true } }, deps),
    "removed-plain",
  );
  assert.equal(
    await removePrunedWorktree({ data: { worktree: { ...handle, dir: "/home/me/precious" } } }, deps),
    "skipped",
  );
  assert.deepEqual(calls, [
    ["git", handle.dir],
    ["rm", handle.dir, { recursive: true, force: true }],
  ]);
});

test("featureIdsToPersist also leaves out ticket-workspace, and keeps ticket-to-pr", () => {
  assert.deepEqual(jobFiles.featureIdsToPersist(["ticket-workspace", "ticket-to-pr"]), ["ticket-to-pr"]);
});
