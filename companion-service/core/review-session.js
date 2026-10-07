// Live status of a terminal-based review (Claude Code / cursor-agent) after
// it has been handed to the terminal. Plain JS so review-session.test.js
// runs with no build step, like the other core/*.js helpers.
//
// The sourced review script (core/terminal.ts's buildTerminalScript)
// appends lines to a per-review status file:
//   shell <pid> <start time>   the user's interactive shell ($$)
//   agent <pid> <start time>   a small `sh` the agent runs under, which
//                              records itself before starting the agent
//   exit <code>                once the agent has exited
// and stopSession appends `cancel`. Start times are `ps -o lstart=` under
// LC_ALL=C on both sides, so a recycled PID never matches a record.
//
// Next to it, a per-worktree session record (JSON) says which job's
// review is using that worktree, with what the panel needs to show it and
// where its terminal is, so a restarted service can still find it. The
// record never names a path: the status file is derived from its job id.

const { createHash, randomUUID } = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFile } = require("child_process");
const { isValidTerminalLocation } = require("./terminal-focus.js");

const STATE_DIR_NAME = "ai-dev-companion-reviews";
const STALE_MS = 24 * 60 * 60 * 1000;
/** How long the terminal gets to start sourcing the script before a
 * review with no shell record counts as never having started. */
const START_TIMEOUT_MS = 2 * 60 * 1000;
const LIVE_STATES = ["starting", "running"];
const JOB_ID = /^[0-9a-f-]{8,64}$/i;

/** The private directory status files live in, created 0700. Refuses a
 * path that isn't a real directory owned by this user. */
function stateDir(base = os.tmpdir()) {
  const dir = path.join(base, STATE_DIR_NAME);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const st = fs.lstatSync(dir);
  if (!st.isDirectory() || st.isSymbolicLink() || (process.getuid && st.uid !== process.getuid())) {
    throw new Error(`${dir} isn't a directory owned by this user; refusing to track reviews there.`);
  }
  if ((st.mode & 0o077) !== 0) fs.chmodSync(dir, 0o700);
  return dir;
}

/** Creates an empty 0600 status file for `jobId`, first deleting any left
 * over for more than a day, and returns its path. */
function createStatusFile(jobId, { dir = stateDir(), now = Date.now() } = {}) {
  pruneStatusFiles(dir, now);
  const file = statusFileFor(jobId, dir);
  fs.writeFileSync(file, "", { mode: 0o600, flag: "wx" });
  return file;
}

function statusFileFor(jobId, dir = stateDir()) {
  if (!JOB_ID.test(String(jobId))) throw new Error(`Invalid job id: ${jobId}`);
  return path.join(dir, `${jobId}.status`);
}

function pruneStatusFiles(dir, now = Date.now()) {
  for (const name of fs.readdirSync(dir)) {
    if (!name.endsWith(".status") && !name.endsWith(".json")) continue;
    const file = path.join(dir, name);
    try {
      if (now - fs.statSync(file).mtimeMs > STALE_MS) fs.unlinkSync(file);
    } catch {
      // Already gone.
    }
  }
}

function parseStatusFile(text) {
  const record = { shell: null, agent: null, exitCode: null, cancelled: false };
  for (const line of text.split("\n")) {
    const proc = /^(shell|agent) (\d+) (.+)$/.exec(line);
    if (proc) {
      record[proc[1]] = { pid: Number(proc[2]), started: proc[3].trim() };
    } else if (/^exit \d+$/.test(line)) {
      record.exitCode = Number(line.slice(5));
    } else if (line === "cancel") {
      record.cancelled = true;
    }
  }
  return record;
}

/**
 * { state, exitCode? } from a parsed status file and whether its recorded
 * processes are still the same live processes:
 *   starting   the terminal hasn't reached the agent yet
 *   running    the agent is running
 *   finished   the agent exited on its own (exitCode)
 *   cancelled  stopSession stopped it
 *   ended      the shell went away without the agent exiting first, i.e.
 *              the terminal was closed (or never started the review)
 */
function computeStatus(record, { shellAlive, agentAlive, ageMs }) {
  if (record.exitCode !== null) {
    return { state: record.cancelled ? "cancelled" : "finished", exitCode: record.exitCode };
  }
  if (!record.shell) return { state: ageMs > START_TIMEOUT_MS ? "ended" : "starting" };
  if (!shellAlive) return { state: record.cancelled ? "cancelled" : "ended" };
  if (record.cancelled && !agentAlive) return { state: "cancelled" };
  return { state: record.agent ? "running" : "starting" };
}

function isLive(status) {
  return !!status && LIVE_STATES.includes(status.state);
}

/** Every descendant of `rootPid`, as { pid, started }, in a
 * `ps -A -o pid=,ppid=,lstart=` listing. */
function descendantsOf(rootPid, psOutput) {
  const children = new Map();
  for (const line of psOutput.split("\n")) {
    const m = /^\s*(\d+)\s+(\d+)\s+(.+?)\s*$/.exec(line);
    if (!m) continue;
    const ppid = Number(m[2]);
    if (!children.has(ppid)) children.set(ppid, []);
    children.get(ppid).push({ pid: Number(m[1]), started: m[3] });
  }
  const found = [];
  const queue = [rootPid];
  while (queue.length > 0) {
    for (const child of children.get(queue.shift()) || []) {
      if (child.pid === rootPid || found.some((p) => p.pid === child.pid)) continue;
      found.push(child);
      queue.push(child.pid);
    }
  }
  return found;
}

function ps(args) {
  return new Promise((resolve) => {
    execFile("ps", args, { env: { ...process.env, LC_ALL: "C" } }, (err, stdout) => resolve(err ? "" : stdout));
  });
}

/** Whether `proc` ({ pid, started }) is still that same process. */
async function isSameProcess(proc) {
  if (!proc) return false;
  return (await ps(["-o", "lstart=", "-p", String(proc.pid)])).trim() === proc.started;
}

async function readStatus(file, { now = Date.now() } = {}) {
  let text;
  let createdMs;
  try {
    text = fs.readFileSync(file, "utf8");
    const st = fs.statSync(file);
    createdMs = st.birthtimeMs || st.mtimeMs;
  } catch {
    return { state: "ended" };
  }
  const record = parseStatusFile(text);
  const [shellAlive, agentAlive] = await Promise.all([isSameProcess(record.shell), isSameProcess(record.agent)]);
  return computeStatus(record, { shellAlive, agentAlive, ageMs: now - createdMs });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Those of `procs` that are still the same processes. */
async function stillRunning(procs) {
  const live = await Promise.all(procs.map(isSameProcess));
  return procs.filter((_, i) => live[i]);
}

async function signalAll(procs, signal) {
  for (const { pid } of await stillRunning(procs)) {
    try {
      process.kill(pid, signal);
    } catch {
      // Exited in the meantime.
    }
  }
}

async function waitForExit(procs, timeoutMs) {
  for (let waited = 0; waited < timeoutMs; waited += 200) {
    if ((await stillRunning(procs)).length === 0) return true;
    await sleep(200);
  }
  return (await stillRunning(procs)).length === 0;
}

/**
 * Stops a running review: SIGINT to the agent and everything below it,
 * then SIGTERM after `graceMs` to whatever is left. The process tree is
 * taken (with start times) up front, after checking the recorded wrapper
 * is still that process and still a child of the recorded shell, because
 * the agent's own children are reparented once it exits. Neither the
 * shell nor the wrapper is signalled, so the script carries on and leaves
 * the terminal at a prompt in the worktree.
 */
async function stopSession(file, { graceMs = 2000 } = {}) {
  const record = parseStatusFile(fs.readFileSync(file, "utf8"));
  if (record.exitCode !== null) return;
  const { shell, agent } = record;
  if (!agent || !(await isSameProcess(shell)) || !(await isSameProcess(agent))) {
    throw new Error("The review isn't running any more.");
  }
  if (Number((await ps(["-o", "ppid=", "-p", String(agent.pid)])).trim()) !== shell.pid) {
    throw new Error("The review's agent is no longer attached to its terminal; not stopping it.");
  }
  const tree = descendantsOf(agent.pid, await ps(["-A", "-o", "pid=,ppid=,lstart="]));
  fs.appendFileSync(file, "cancel\n");
  await signalAll(tree, "SIGINT");
  if (await waitForExit(tree, graceMs)) return;
  await signalAll(tree, "SIGTERM");
  if (!(await waitForExit(tree, graceMs))) {
    throw new Error("The review's agent didn't stop; close its terminal window to end it.");
  }
}

/** Polls until the review in `file` is no longer live, for at most
 * `timeoutMs`; resolves its last status. */
async function waitUntilStopped(file, { timeoutMs = 10000, intervalMs = 250 } = {}) {
  let status = await readStatus(file);
  for (let waited = 0; isLive(status) && waited < timeoutMs; waited += intervalMs) {
    await sleep(intervalMs);
    status = await readStatus(file);
  }
  return status;
}

function sessionRecordFile(worktree, dir = stateDir()) {
  const hash = createHash("sha256").update(path.resolve(worktree)).digest("hex").slice(0, 24);
  return path.join(dir, `worktree-${hash}.json`);
}

/** Saves `record` ({ jobId, worktree, payload, summary, review, terminal? })
 * as its worktree's session, replacing any earlier one. */
function writeSessionRecord(record, { dir = stateDir() } = {}) {
  statusFileFor(record.jobId, dir);
  const worktree = path.resolve(record.worktree);
  const file = sessionRecordFile(worktree, dir);
  const tmp = `${file}.${randomUUID()}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ ...record, worktree }), { mode: 0o600, flag: "wx" });
  fs.renameSync(tmp, file);
}

/** A session record's fields, or null unless it's well-formed and for
 * `worktree`. An invalid terminal location is dropped. */
function parseSessionRecord(text, worktree) {
  let r;
  try {
    r = JSON.parse(text);
  } catch {
    return null;
  }
  if (!r || typeof r !== "object" || !JOB_ID.test(String(r.jobId)) || r.worktree !== path.resolve(worktree)) {
    return null;
  }
  if (!r.payload || typeof r.payload !== "object" || !r.review || typeof r.review !== "object") return null;
  return {
    jobId: r.jobId,
    worktree: r.worktree,
    payload: r.payload,
    summary: typeof r.summary === "string" ? r.summary : "",
    review: r.review,
    terminal: isValidTerminalLocation(r.terminal) ? r.terminal : undefined,
  };
}

function readSessionRecord(worktree, { dir = stateDir() } = {}) {
  try {
    return parseSessionRecord(fs.readFileSync(sessionRecordFile(worktree, dir), "utf8"), worktree);
  } catch {
    return null;
  }
}

/** Every well-formed session record in `dir`. */
function listSessionRecords({ dir = stateDir() } = {}) {
  const records = [];
  for (const name of fs.readdirSync(dir)) {
    if (!/^worktree-[0-9a-f]{24}\.json$/.test(name)) continue;
    let worktree;
    try {
      worktree = JSON.parse(fs.readFileSync(path.join(dir, name), "utf8")).worktree;
    } catch {
      continue;
    }
    if (typeof worktree !== "string" || path.basename(sessionRecordFile(worktree, dir)) !== name) continue;
    const record = readSessionRecord(worktree, { dir });
    if (record) records.push(record);
  }
  return records;
}

/** Deletes `worktree`'s session record if it's still `jobId`'s. */
function removeSessionRecord(worktree, jobId, { dir = stateDir() } = {}) {
  if (readSessionRecord(worktree, { dir })?.jobId === jobId) {
    fs.rmSync(sessionRecordFile(worktree, dir), { force: true });
  }
}

/** `worktree`'s review if it's still live: { record, status }, else null. */
async function findLiveSession(worktree, { dir = stateDir() } = {}) {
  const record = readSessionRecord(worktree, { dir });
  if (!record) return null;
  const status = await readStatus(statusFileFor(record.jobId, dir));
  return isLive(status) ? { record, status } : null;
}

module.exports = {
  STATE_DIR_NAME,
  START_TIMEOUT_MS,
  stateDir,
  createStatusFile,
  statusFileFor,
  pruneStatusFiles,
  parseStatusFile,
  computeStatus,
  isLive,
  descendantsOf,
  isSameProcess,
  readStatus,
  stopSession,
  waitUntilStopped,
  sessionRecordFile,
  writeSessionRecord,
  parseSessionRecord,
  readSessionRecord,
  listSessionRecords,
  removeSessionRecord,
  findLiveSession,
};
