const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const jobFiles = require("./job-files.js");

const ID = "11111111-1111-1111-1111-111111111111";
const ID_2 = "22222222-2222-2222-2222-222222222222";
const ID_3 = "33333333-3333-3333-3333-333333333333";
const ID_4 = "44444444-4444-4444-4444-444444444444";
const ID_5 = "55555555-5555-5555-5555-555555555555";

/** A fresh temp root for one test, cleaned up by the caller. */
function tmp() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "job-files-"));
}

/** A minimal Job (core/jobs.ts's shape) with a fixed id, for one test to
 * override as needed. */
function job(overrides = {}) {
  return {
    id: ID,
    featureId: "review-in-editor",
    status: "awaiting-approval",
    data: {},
    createdAt: 1000,
    updatedAt: 1000,
    ...overrides,
  };
}

test("saveJob then loadJobs round-trips an awaiting-approval job unchanged", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const j = job();
  jobFiles.saveJob(dir, j);

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot: root });

  assert.deepEqual(result, { jobs: [j], reconciled: [], pruned: [], prunedJobs: [], dropped: [] });
  assert.equal(fs.statSync(dir).mode & 0o777, 0o700);
  assert.equal(fs.statSync(jobFiles.jobFile(dir, j.id)).mode & 0o777, 0o600);
  fs.rmSync(root, { recursive: true, force: true });
});

test("loadJobs on a missing dir returns empty arrays and never throws", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);

  const result = jobFiles.loadJobs(dir, { worktreeRoot: root });

  assert.deepEqual(result, { jobs: [], reconciled: [], pruned: [], prunedJobs: [], dropped: [] });
  fs.rmSync(root, { recursive: true, force: true });
});

/** `data.worktree` laid out exactly as core/worktree.ts's addWorktree
 * creates it — `<worktreeRoot>/<repo-slug>/<id>` — the only shape loadJobs
 * accepts. */
function worktreeData(worktreeRoot, id = ID) {
  return { worktree: { dir: path.join(worktreeRoot, "some-repo", id) } };
}

for (const status of ["running", "approving", "rejecting"]) {
  test(`loadJobs reconciles a ${status} job to failed with the restart message and drops progress`, () => {
    const root = tmp();
    const dir = jobFiles.jobsDir(root);
    const worktreeRoot = path.join(root, "worktrees");
    const j = job({ status, data: worktreeData(worktreeRoot), progress: { stepId: "merge", label: "Merging" } });
    jobFiles.saveJob(dir, j);

    const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot });

    assert.equal(result.jobs.length, 1);
    const reconciled = result.jobs[0];
    assert.equal(reconciled.status, "failed");
    assert.equal(reconciled.error, jobFiles.RESTART_MESSAGE);
    assert.ok(reconciled.error.includes("service restarted — click Refresh diff"));
    assert.equal(reconciled.progress, undefined);
    assert.equal(reconciled.updatedAt, 2000);
    assert.deepEqual(result.reconciled, [j.id]);

    const reload = jobFiles.loadJobs(dir, { now: 3000, worktreeRoot });
    assert.deepEqual(reload.jobs, result.jobs);
    fs.rmSync(root, { recursive: true, force: true });
  });
}

test("an interrupted job with no worktree gets RESTART_MESSAGE_NO_WORKTREE, not the Refresh diff one", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  jobFiles.saveJob(dir, job({ status: "running", progress: { stepId: "fetch", label: "Fetching" } }));

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot: path.join(root, "worktrees") });

  assert.equal(
    jobFiles.RESTART_MESSAGE_NO_WORKTREE,
    "The service restarted before this job had anything to review (service restarted — start it again).",
  );
  assert.equal(result.jobs.length, 1);
  assert.equal(result.jobs[0].status, "failed");
  assert.equal(result.jobs[0].error, jobFiles.RESTART_MESSAGE_NO_WORKTREE);
  assert.equal(result.jobs[0].progress, undefined);
  assert.deepEqual(result.reconciled, [ID]);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a stale job is pruned; both files disappear but its worktree dir is left on disk", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeRoot = path.join(root, "worktrees");
  const worktreeDir = path.join(worktreeRoot, "some-repo", ID);
  fs.mkdirSync(worktreeDir, { recursive: true });
  const now = 100_000_000;
  const j = job({ data: { worktree: { dir: worktreeDir } }, updatedAt: now - jobFiles.PRUNE_AFTER_MS - 1 });
  jobFiles.saveJob(dir, j);
  jobFiles.writeJsonAtomic(jobFiles.baselineFile(dir, j.id), { some: "baseline" });
  const logged = [];

  const result = jobFiles.loadJobs(dir, { now, worktreeRoot, log: (msg) => logged.push(msg) });

  assert.deepEqual(result.jobs, []);
  assert.deepEqual(result.pruned, [j.id]);
  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, j.id)));
  assert.ok(!fs.existsSync(jobFiles.baselineFile(dir, j.id)));
  assert.ok(fs.existsSync(worktreeDir));
  assert.ok(logged.some((m) => m.includes(`pruned stale job ${j.id}`) && m.includes(worktreeDir)));
  fs.rmSync(root, { recursive: true, force: true });
});

test("a job whose worktree.dir is outside worktreeRoot is dropped, including a sibling, the root itself, and a .. escape", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeRoot = path.join(root, "worktrees");
  fs.mkdirSync(worktreeRoot, { recursive: true });

  const cases = [
    [ID, path.join(root, "sibling")],
    [ID_2, worktreeRoot],
    [ID_3, path.join(worktreeRoot, "..", "escape")],
  ];
  for (const [id, badDir] of cases) {
    jobFiles.saveJob(dir, job({ id, data: { worktree: { dir: badDir } } }));
  }

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot });

  assert.deepEqual(result.jobs, []);
  assert.deepEqual(result.dropped.sort(), [ID, ID_2, ID_3].sort());
  for (const [id] of cases) {
    assert.ok(!fs.existsSync(jobFiles.jobFile(dir, id)));
  }
  fs.rmSync(root, { recursive: true, force: true });
});

test("a worktree in the real <root>/<repo>/<id> layout, with a matching claudeSession cwd, loads", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeRoot = path.join(root, "worktrees");
  const data = worktreeData(worktreeRoot);
  data.claudeSession = { id: "s", cwd: data.worktree.dir, permissionMode: "default" };
  const j = job({ data });
  jobFiles.saveJob(dir, j);

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot });

  assert.deepEqual(result.jobs, [j]);
  assert.deepEqual(result.dropped, []);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a worktree that isn't exactly <worktreeRoot>/<repo>/<job id> is dropped, along with its baseline", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeRoot = path.join(root, "worktrees");

  const cases = [
    // An empty dir.
    [ID, { worktree: { dir: "" } }],
    // A worktree object with no dir at all.
    [ID_2, { worktree: {} }],
    // The right depth, but named for some other job.
    [ID_3, { worktree: { dir: path.join(worktreeRoot, "some-repo", ID_5) } }],
    // Directly under the root: a whole repo's worktrees dir, not one job's.
    [ID_4, { worktree: { dir: path.join(worktreeRoot, ID_4) } }],
  ];
  for (const [id, data] of cases) {
    jobFiles.saveJob(dir, job({ id, data }));
    jobFiles.writeJsonAtomic(jobFiles.baselineFile(dir, id), { some: "baseline" });
  }
  const logged = [];

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot, log: (m) => logged.push(m) });

  assert.deepEqual(result.jobs, []);
  assert.deepEqual(result.dropped.sort(), cases.map(([id]) => id).sort());
  for (const [id] of cases) {
    assert.ok(!fs.existsSync(jobFiles.jobFile(dir, id)));
    assert.ok(!fs.existsSync(jobFiles.baselineFile(dir, id)));
  }
  assert.ok(logged.length >= cases.length);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a claudeSession whose cwd isn't the job's worktree dir is dropped, along with its baseline", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeRoot = path.join(root, "worktrees");
  const mismatched = worktreeData(worktreeRoot, ID);
  mismatched.claudeSession = { id: "s", cwd: path.join(root, "elsewhere"), permissionMode: "default" };
  // A session with no worktree at all can't match one either.
  const noWorktree = {
    claudeSession: { id: "s", cwd: path.join(worktreeRoot, "some-repo", ID_2), permissionMode: "default" },
  };
  jobFiles.saveJob(dir, job({ id: ID, data: mismatched }));
  jobFiles.saveJob(dir, job({ id: ID_2, data: noWorktree }));
  jobFiles.writeJsonAtomic(jobFiles.baselineFile(dir, ID), { some: "baseline" });

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot });

  assert.deepEqual(result.jobs, []);
  assert.deepEqual(result.dropped.sort(), [ID, ID_2].sort());
  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, ID)));
  assert.ok(!fs.existsSync(jobFiles.baselineFile(dir, ID)));
  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, ID_2)));
  fs.rmSync(root, { recursive: true, force: true });
});

test("garbage JSON, a non-UUID id, and a name/id mismatch are each dropped without throwing; other valid jobs still load", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });

  // Garbage JSON.
  fs.writeFileSync(path.join(dir, `${ID}.json`), "not json{", { mode: 0o600 });
  // A non-UUID id (both the filename and the id field).
  fs.writeFileSync(path.join(dir, "not-a-uuid.json"), JSON.stringify(job({ id: "not-a-uuid" })), { mode: 0o600 });
  // A valid UUID id that doesn't match its own file name.
  fs.writeFileSync(path.join(dir, `${ID_2}.json`), JSON.stringify(job({ id: ID_3 })), { mode: 0o600 });
  // A genuinely valid job, which should still load.
  jobFiles.saveJob(dir, job({ id: ID_4 }));

  const logged = [];
  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot: root, log: (m) => logged.push(m) });

  assert.deepEqual(result.jobs, [job({ id: ID_4 })]);
  assert.deepEqual(result.dropped.sort(), [ID, "not-a-uuid", ID_2].sort());
  assert.ok(!fs.existsSync(path.join(dir, `${ID}.json`)));
  assert.ok(!fs.existsSync(path.join(dir, "not-a-uuid.json")));
  assert.ok(!fs.existsSync(path.join(dir, `${ID_2}.json`)));
  assert.ok(logged.length >= 3);
  fs.rmSync(root, { recursive: true, force: true });
});

test("loadJobs never throws when worktreeRoot is missing while a job has data.worktree.dir; other valid jobs still load", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeJob = job({ id: ID, data: { worktree: { dir: path.join(root, "somewhere") } } });
  const plainJob = job({ id: ID_2 });
  jobFiles.saveJob(dir, worktreeJob);
  jobFiles.saveJob(dir, plainJob);
  const logged = [];

  const result = jobFiles.loadJobs(dir, { now: 2000, log: (m) => logged.push(m) });

  assert.deepEqual(result.jobs, [plainJob]);
  assert.deepEqual(result.dropped, [ID]);
  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, ID)));
  assert.ok(logged.length >= 1);
  fs.rmSync(root, { recursive: true, force: true });
});

test("loadJobs never throws when the reconcile save fails; the job still comes back failed in memory", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  const worktreeRoot = path.join(root, "worktrees");
  const j = job({ status: "running", data: worktreeData(worktreeRoot), progress: { stepId: "merge", label: "Merging" } });
  jobFiles.saveJob(dir, j);
  fs.chmodSync(dir, 0o500);
  const logged = [];

  try {
    const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot, log: (m) => logged.push(m) });

    assert.equal(result.jobs.length, 1);
    assert.equal(result.jobs[0].status, "failed");
    assert.ok(result.jobs[0].error.includes("service restarted — click Refresh diff"));
    assert.equal(result.jobs[0].progress, undefined);
    assert.deepEqual(result.reconciled, [j.id]);
    assert.ok(logged.some((m) => m.includes(j.id)));
  } finally {
    fs.chmodSync(dir, 0o700);
  }
  fs.rmSync(root, { recursive: true, force: true });
});

test("loadJobs drops a job whose updatedAt isn't a finite number, removing its files", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  jobFiles.saveJob(dir, job({ id: ID, updatedAt: undefined }));
  jobFiles.saveJob(dir, job({ id: ID_2, updatedAt: Number.NaN }));
  jobFiles.saveJob(dir, job({ id: ID_3 }));
  const logged = [];

  const result = jobFiles.loadJobs(dir, { now: 2000, worktreeRoot: root, log: (m) => logged.push(m) });

  assert.deepEqual(result.jobs, [job({ id: ID_3 })]);
  assert.deepEqual(result.dropped.sort(), [ID, ID_2].sort());
  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, ID)));
  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, ID_2)));
  fs.rmSync(root, { recursive: true, force: true });
});

test("an orphan <id>.integrity.json is removed; a baseline with a matching job file is kept", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  jobFiles.saveJob(dir, job({ id: ID }));
  jobFiles.writeJsonAtomic(jobFiles.baselineFile(dir, ID), { kept: true });
  jobFiles.writeJsonAtomic(jobFiles.baselineFile(dir, ID_2), { orphan: true });

  jobFiles.loadJobs(dir, { now: 2000, worktreeRoot: root });

  assert.ok(fs.existsSync(jobFiles.baselineFile(dir, ID)));
  assert.ok(!fs.existsSync(jobFiles.baselineFile(dir, ID_2)));
  fs.rmSync(root, { recursive: true, force: true });
});

test("removeJob removes both files and is fine when neither exists", () => {
  const root = tmp();
  const dir = jobFiles.jobsDir(root);
  jobFiles.saveJob(dir, job({ id: ID_5 }));
  jobFiles.writeJsonAtomic(jobFiles.baselineFile(dir, ID_5), { a: 1 });

  jobFiles.removeJob(dir, ID_5);

  assert.ok(!fs.existsSync(jobFiles.jobFile(dir, ID_5)));
  assert.ok(!fs.existsSync(jobFiles.baselineFile(dir, ID_5)));
  assert.doesNotThrow(() => jobFiles.removeJob(dir, ID_5));
  fs.rmSync(root, { recursive: true, force: true });
});

test("jobFile throws on a non-UUID id", () => {
  assert.throws(() => jobFiles.jobFile("/tmp/whatever", "../x"));
});

test("baselineFile throws on a non-UUID id and otherwise names <id>.integrity.json", () => {
  assert.throws(() => jobFiles.baselineFile("/tmp/whatever", "../x"));
  assert.equal(jobFiles.baselineFile("/tmp/whatever", ID), path.join("/tmp/whatever", `${ID}.integrity.json`));
});

test("persistAction returns skip, save, and remove as specified", () => {
  const featureIds = ["review-in-editor"];
  assert.equal(jobFiles.persistAction(job({ featureId: "other-feature" }), featureIds), "skip");
  assert.equal(jobFiles.persistAction(job({ status: "approved" }), featureIds), "remove");
  assert.equal(jobFiles.persistAction(job({ status: "rejected" }), featureIds), "remove");
  assert.equal(jobFiles.persistAction(job({ status: "awaiting-approval" }), featureIds), "save");
  assert.equal(jobFiles.persistAction(job({ status: "running" }), featureIds), "save");
});

test("writeJsonAtomic leaves no .tmp file behind", () => {
  const root = tmp();
  const file = path.join(root, "sub", "thing.json");

  jobFiles.writeJsonAtomic(file, { a: 1 });

  const entries = fs.readdirSync(path.dirname(file));
  assert.ok(!entries.some((n) => n.endsWith(".tmp")));
  assert.deepEqual(JSON.parse(fs.readFileSync(file, "utf8")), { a: 1 });
  assert.equal(fs.statSync(path.dirname(file)).mode & 0o777, 0o700);
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  fs.rmSync(root, { recursive: true, force: true });
});
