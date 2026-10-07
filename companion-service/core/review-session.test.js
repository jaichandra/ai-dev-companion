const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const {
  START_TIMEOUT_MS,
  stateDir,
  createStatusFile,
  parseStatusFile,
  computeStatus,
  isLive,
  descendantsOf,
  statusFileFor,
  sessionRecordFile,
  writeSessionRecord,
  parseSessionRecord,
  readSessionRecord,
  listSessionRecords,
  removeSessionRecord,
  findLiveSession,
} = require("./review-session.js");

const STARTED = "Thu Sep 24 16:19:34 2026";
const record = (lines) => parseStatusFile(lines.join("\n") + "\n");

test("parseStatusFile reads the shell, agent, exit code and cancel lines", () => {
  assert.deepEqual(record([`shell 101 ${STARTED}`, `agent 202 ${STARTED}`, "exit 130", "cancel"]), {
    shell: { pid: 101, started: STARTED },
    agent: { pid: 202, started: STARTED },
    exitCode: 130,
    cancelled: true,
  });
});

test("parseStatusFile ignores anything it doesn't recognise", () => {
  assert.deepEqual(record(["", "shell x y", "exit", "cancelled", "agent 5"]), {
    shell: null,
    agent: null,
    exitCode: null,
    cancelled: false,
  });
});

test("computeStatus: no shell yet is starting, until the start timeout", () => {
  const rec = record([]);
  assert.deepEqual(computeStatus(rec, { ageMs: 1000 }), { state: "starting" });
  assert.deepEqual(computeStatus(rec, { ageMs: START_TIMEOUT_MS + 1 }), { state: "ended" });
});

test("computeStatus: shell alive, agent not recorded yet, is starting", () => {
  assert.deepEqual(computeStatus(record([`shell 1 ${STARTED}`]), { shellAlive: true, ageMs: 0 }), {
    state: "starting",
  });
});

test("computeStatus: a live agent is running", () => {
  const rec = record([`shell 1 ${STARTED}`, `agent 2 ${STARTED}`]);
  assert.deepEqual(computeStatus(rec, { shellAlive: true, agentAlive: true, ageMs: 0 }), { state: "running" });
});

test("computeStatus: an exit line is finished, with its code", () => {
  const rec = record([`shell 1 ${STARTED}`, `agent 2 ${STARTED}`, "exit 0"]);
  assert.deepEqual(computeStatus(rec, { shellAlive: true, agentAlive: false, ageMs: 0 }), {
    state: "finished",
    exitCode: 0,
  });
});

test("computeStatus: the shell gone without an exit line means the terminal was closed", () => {
  const rec = record([`shell 1 ${STARTED}`, `agent 2 ${STARTED}`]);
  assert.deepEqual(computeStatus(rec, { shellAlive: false, agentAlive: false, ageMs: 0 }), { state: "ended" });
  assert.deepEqual(computeStatus(rec, { shellAlive: false, agentAlive: true, ageMs: 0 }), { state: "ended" });
});

test("computeStatus: a cancel is cancelled once the agent is gone, however the script ended", () => {
  const base = [`shell 1 ${STARTED}`, `agent 2 ${STARTED}`, "cancel"];
  assert.deepEqual(computeStatus(record([...base, "exit 130"]), { shellAlive: true, ageMs: 0 }), {
    state: "cancelled",
    exitCode: 130,
  });
  assert.deepEqual(computeStatus(record(base), { shellAlive: true, agentAlive: false, ageMs: 0 }), {
    state: "cancelled",
  });
  assert.deepEqual(computeStatus(record(base), { shellAlive: false, ageMs: 0 }), { state: "cancelled" });
});

test("computeStatus: a cancel that hasn't taken effect yet is still running", () => {
  const rec = record([`shell 1 ${STARTED}`, `agent 2 ${STARTED}`, "cancel"]);
  assert.deepEqual(computeStatus(rec, { shellAlive: true, agentAlive: true, ageMs: 0 }), { state: "running" });
});

test("isLive is true only for starting and running", () => {
  assert.equal(isLive({ state: "starting" }), true);
  assert.equal(isLive({ state: "running" }), true);
  for (const state of ["finished", "cancelled", "ended"]) assert.equal(isLive({ state }), false);
  assert.equal(isLive(undefined), false);
});

test("descendantsOf walks the whole process tree below a pid, with start times, and nothing else", () => {
  const rows = [
    [1, 0],
    [10, 1],
    [20, 10],
    [21, 10],
    [30, 20],
    [40, 1],
    [50, 40],
  ];
  const ps = rows.map(([pid, ppid]) => `  ${pid}     ${ppid} ${STARTED}`).join("\n") + "\n";
  const found = descendantsOf(10, ps).sort((a, b) => a.pid - b.pid);
  assert.deepEqual(found.map((p) => p.pid), [20, 21, 30]);
  assert.ok(found.every((p) => p.started === STARTED));
  assert.deepEqual(descendantsOf(30, ps), []);
});

test("stateDir creates a private directory and createStatusFile an empty 0600 file in it", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  assert.equal(fs.statSync(dir).mode & 0o777, 0o700);
  const file = createStatusFile("0a1b2c3d-0000-4000-8000-000000000000", { dir });
  assert.equal(path.dirname(file), dir);
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.equal(fs.readFileSync(file, "utf8"), "");
});

test("stateDir tightens a directory that's readable by others", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  fs.mkdirSync(path.join(base, "ai-dev-companion-reviews"), { mode: 0o755 });
  assert.equal(fs.statSync(stateDir(base)).mode & 0o777, 0o700);
});

test("stateDir refuses a symlink", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  fs.mkdirSync(path.join(base, "elsewhere"));
  fs.symlinkSync(path.join(base, "elsewhere"), path.join(base, "ai-dev-companion-reviews"));
  assert.throws(() => stateDir(base), /refusing/);
});

test("createStatusFile rejects a job id that isn't a plain id, and prunes day-old files", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  assert.throws(() => createStatusFile("../../etc/passwd", { dir }), /Invalid job id/);
  const old = path.join(dir, "deadbeef.status");
  fs.writeFileSync(old, "");
  const dayAgo = (Date.now() - 25 * 60 * 60 * 1000) / 1000;
  fs.utimesSync(old, dayAgo, dayAgo);
  createStatusFile("cafebabe", { dir });
  assert.equal(fs.existsSync(old), false);
});

const JOB = "0a1b2c3d-0000-4000-8000-000000000001";
const sessionRecord = (worktree, extra = {}) => ({
  jobId: JOB,
  worktree,
  payload: { project: "ACME", repo: "sample-app", prId: 5429, sourceBranch: "feature" },
  summary: "Checked out feature.",
  review: { prId: 5429, tracked: true },
  terminal: { app: "iterm", windowId: "7", sessionId: "0F3B5B28-6C1D-4E0A-9E2B-5A1C2D3E4F50" },
  ...extra,
});

test("session records are 0600, keyed by worktree, and read back as written", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  writeSessionRecord(sessionRecord("/wt/a/"), { dir });
  const file = sessionRecordFile("/wt/a", dir);
  assert.equal(path.dirname(file), dir);
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.deepEqual(readSessionRecord("/wt/a", { dir }), sessionRecord("/wt/a"));
  assert.equal(readSessionRecord("/wt/b", { dir }), null);
  assert.deepEqual(fs.readdirSync(dir), [path.basename(file)]);
});

test("writeSessionRecord replaces the worktree's earlier record and rejects a bad job id", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  writeSessionRecord(sessionRecord("/wt/a"), { dir });
  const next = "0a1b2c3d-0000-4000-8000-000000000002";
  writeSessionRecord(sessionRecord("/wt/a", { jobId: next }), { dir });
  assert.equal(readSessionRecord("/wt/a", { dir }).jobId, next);
  assert.throws(() => writeSessionRecord(sessionRecord("/wt/a", { jobId: "../x" }), { dir }), /Invalid job id/);
});

test("parseSessionRecord refuses malformed records and drops an invalid terminal", () => {
  const good = sessionRecord("/wt/a");
  assert.equal(parseSessionRecord("not json", "/wt/a"), null);
  assert.equal(parseSessionRecord(JSON.stringify(good), "/wt/other"), null);
  assert.equal(parseSessionRecord(JSON.stringify({ ...good, jobId: "../../x" }), "/wt/a"), null);
  assert.equal(parseSessionRecord(JSON.stringify({ ...good, payload: null }), "/wt/a"), null);
  const badTerminal = { app: "terminal", windowId: "1", tty: '/dev/ttys1" & quit' };
  assert.equal(parseSessionRecord(JSON.stringify({ ...good, terminal: badTerminal }), "/wt/a").terminal, undefined);
});

test("listSessionRecords returns only well-formed records under their own names", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  writeSessionRecord(sessionRecord("/wt/a"), { dir });
  fs.writeFileSync(path.join(dir, "worktree-000000000000000000000000.json"), JSON.stringify(sessionRecord("/wt/b")));
  fs.writeFileSync(path.join(dir, "worktree-111111111111111111111111.json"), "{");
  assert.deepEqual(
    listSessionRecords({ dir }).map((r) => r.worktree),
    ["/wt/a"],
  );
});

test("removeSessionRecord only removes the record of the given job", (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  writeSessionRecord(sessionRecord("/wt/a"), { dir });
  removeSessionRecord("/wt/a", "0a1b2c3d-0000-4000-8000-00000000ffff", { dir });
  assert.notEqual(readSessionRecord("/wt/a", { dir }), null);
  removeSessionRecord("/wt/a", JOB, { dir });
  assert.equal(readSessionRecord("/wt/a", { dir }), null);
});

test("findLiveSession reports a just-created review as live and a missing one as not", async (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "review-session-test-"));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));
  const dir = stateDir(base);
  assert.equal(await findLiveSession("/wt/a", { dir }), null);
  writeSessionRecord(sessionRecord("/wt/a"), { dir });
  assert.equal(await findLiveSession("/wt/a", { dir }), null);
  createStatusFile(JOB, { dir });
  const live = await findLiveSession("/wt/a", { dir });
  assert.equal(live.status.state, "starting");
  assert.equal(live.record.jobId, JOB);
  fs.writeFileSync(statusFileFor(JOB, dir), "exit 0\n");
  assert.equal(await findLiveSession("/wt/a", { dir }), null);
});
