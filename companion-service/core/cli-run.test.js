const test = require("node:test");
const assert = require("node:assert/strict");
const { main } = require("./cli-run.js");

const SESSION = { id: "3f2b8c1e-1111-4222-8333-444455556666", cwd: "/w/clone", permissionMode: "plan" };

function harness(tools = {}, extra = {}) {
  const out = [];
  const err = [];
  const calls = [];
  const deps = {
    baseUrl: "http://127.0.0.1:8787",
    now: () => 10_000_000,
    home: "/home/me",
    out: (s) => out.push(s),
    err: (s) => err.push(s),
    callTool: async (name, args) => {
      calls.push([name, args]);
      if (!(name in tools)) throw new Error(`unexpected tool ${name}`);
      const v = tools[name];
      if (v instanceof Error) throw v;
      return typeof v === "function" ? v(args) : v;
    },
    exists: () => true,
    cwdAllowed: (cwd) => cwd.startsWith("/w/"),
    runClaude: async (args, cwd) => {
      calls.push(["claude", args, cwd]);
      return 0;
    },
    runDoctor: async () => 0,
    ...extra,
  };
  return { deps, out, err, calls };
}

test("status lists what is running; jobs filters by feature; both accept --json", async () => {
  const jobs = [{ id: "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa", featureId: "analyze-issue", status: "failed", updatedAt: 9_000_000 }];
  const h = harness({ list_jobs: jobs });
  assert.equal(await main(["status"], h.deps), 0);
  assert.match(h.out.join(""), /running at/);
  assert.equal(await main(["jobs", "--feature", "analyze-issue"], h.deps), 0);
  assert.deepEqual(h.calls[1], ["list_jobs", { featureId: "analyze-issue" }]);
  const j = harness({ list_jobs: jobs });
  await main(["jobs", "--json"], j.deps);
  assert.deepEqual(JSON.parse(j.out.join("")), jobs);
});

test("history with a key shows that item; with none it shows the timing metrics; --search searches", async () => {
  const h = harness({
    get_history: { item: { kind: "ticket", key: "jira:PROJ-7", title: "T", updatedAt: 9_000_000 }, edges: [], facts: [] },
    list_history: { days: 30, metrics: [], matches: [] },
  });
  assert.equal(await main(["history", "PROJ-7"], h.deps), 0);
  assert.deepEqual(h.calls[0], ["get_history", { key: "PROJ-7" }]);
  await main(["history"], h.deps);
  assert.deepEqual(h.calls[1], ["list_history", {}]);
  await main(["history", "--search", "login", "--days", "7"], h.deps);
  assert.deepEqual(h.calls[2], ["list_history", { query: "login", days: 7 }]);
});

test("resume runs claude --resume in the session's folder, after the same checks the service makes", async () => {
  const job = { id: "j", featureId: "analyze-issue", status: "awaiting-approval", data: { claudeSession: SESSION } };
  const ok = harness({ get_job: job });
  assert.equal(await main(["resume", "j"], ok.deps), 0);
  assert.deepEqual(ok.calls[1], ["claude", ["--resume", SESSION.id, "--permission-mode", "plan"], "/w/clone"]);

  const running = harness({ get_job: { ...job, status: "running" } });
  assert.equal(await main(["resume", "j"], running.deps), 1);
  assert.match(running.err.join(""), /still running/);

  const none = harness({ get_job: { ...job, data: {} } });
  assert.equal(await main(["resume", "j"], none.deps), 1);
  assert.match(none.err.join(""), /no resumable/);

  const gone = harness({ get_job: job }, { exists: () => false });
  assert.equal(await main(["resume", "j"], gone.deps), 1);
  assert.match(gone.err.join(""), /expired/);
});

test("forget shows what it would delete and needs --yes to do it", async () => {
  const detail = { item: { kind: "ticket", key: "jira:PROJ-7", title: "Login", updatedAt: 9_000_000 }, edges: [{}, {}], facts: [] };
  const h = harness({ get_history: detail, forget_item: { items: 1, events: 2 } });
  assert.equal(await main(["forget", "PROJ-7"], h.deps), 2);
  assert.match(h.out.join(""), /--yes/);
  assert.ok(!h.calls.some(([n]) => n === "forget_item"));
  assert.equal(await main(["forget", "PROJ-7", "--yes"], h.deps), 0);
  assert.deepEqual(h.calls.find(([n]) => n === "forget_item"), ["forget_item", { key: "PROJ-7" }]);
  assert.match(h.out.join(""), /Removed 1 item\(s\) and 2 timing event/);
});

test("errors from the service come out as one plain line and exit code 1; bad usage exits 2", async () => {
  const h = harness({ list_jobs: new Error("The companion service isn't running.") });
  assert.equal(await main(["status"], h.deps), 1);
  assert.match(h.err.join(""), /companion: The companion service isn't running/);
  const bad = harness();
  assert.equal(await main(["frobnicate"], bad.deps), 2);
  assert.equal(await main(["resume"], bad.deps), 2);
  assert.equal(await main([], bad.deps), 0);
  assert.match(bad.out.join(""), /Usage:/);
});

test("forget reports a real failure instead of 'Nothing is recorded'", async () => {
  const h = harness({ get_history: new Error("The companion service isn't running.") });
  assert.equal(await main(["forget", "PROJ-7"], h.deps), 1);
  assert.match(h.err.join(""), /isn't running/);
  assert.doesNotMatch(h.err.join(""), /Nothing is recorded/);
  const h2 = harness({ get_history: new Error("Nothing is recorded under PROJ-7.") });
  assert.equal(await main(["forget", "PROJ-7"], h2.deps), 1);
  assert.match(h2.err.join(""), /Nothing is recorded/);
});

test("history rejects a --days that is not a whole number, without calling the service", async () => {
  for (const bad of ["abc", "0", "1.5"]) {
    const h = harness({ list_history: [] });
    assert.equal(await main(["history", "--days", bad], h.deps), 2);
    assert.match(h.err.join(""), /--days needs a whole number of days/);
    assert.equal(h.calls.length, 0);
  }
});

test("open sends the words as one text to open_location and prints where it opened", async () => {
  const h = harness({ open_location: { repo: "ACME/sample-app", path: "src/app/login.ts", line: 42, column: 13, editor: "cursor" } });
  assert.equal(await main(["open", "src/app/login.ts:42:13"], h.deps), 0);
  assert.deepEqual(h.calls[0], ["open_location", { text: "src/app/login.ts:42:13" }]);
  assert.match(h.out.join(""), /Opened ACME\/sample-app: src\/app\/login\.ts:42:13 in cursor\./);
  const words = harness({ open_location: { repo: "r", path: "a.ts", line: 1, column: null, editor: "cursor" } });
  assert.equal(await main(["open", "at", "f", "(a.ts:1)"], words.deps), 0);
  assert.deepEqual(words.calls, [["open_location", { text: "at f (a.ts:1)" }]]);
  const bad = harness({});
  assert.equal(await main(["open"], bad.deps), 2);
  assert.equal(bad.calls.length, 0);
  assert.match(bad.err.join(""), /Open what\? Give a location such as src\/app\/login\.ts:42\./);
});

test("resume <ticket key> resumes the ticket's session, else opens Claude in its worktree, else says why not", async () => {
  const WT = "/w/sample-app.worktrees/PROJ-7";
  const ws = (resumeTarget) => ({ issueKey: "PROJ-7", resume: resumeTarget });
  const withSession = harness({ ticket_workspace: ws({ session: { ...SESSION, cwd: WT }, worktreeDir: WT }) });
  assert.equal(await main(["resume", "proj-7"], withSession.deps), 0);
  assert.deepEqual(withSession.calls[0], ["ticket_workspace", { issueKey: "PROJ-7" }]);
  assert.deepEqual(withSession.calls[1], ["claude", ["--resume", SESSION.id, "--permission-mode", "plan"], WT]);

  // The transcript is gone: fall back to a fresh session in the worktree.
  const expired = harness(
    { ticket_workspace: ws({ session: { ...SESSION, cwd: WT }, worktreeDir: WT }) },
    { exists: (p) => !p.endsWith(".jsonl") },
  );
  assert.equal(await main(["resume", "PROJ-7"], expired.deps), 0);
  assert.deepEqual(expired.calls[1], ["claude", [], WT]);
  assert.match(expired.out.join(""), /No resumable Claude session/);

  const nothing = harness({ ticket_workspace: ws({ session: null, worktreeDir: null }) });
  assert.equal(await main(["resume", "PROJ-7"], nothing.deps), 1);
  assert.match(nothing.err.join(""), /Nothing to resume for PROJ-7/);
});

test("resume <ticket key>: a folder the allow-list refuses is never entered; the fallback and exit codes hold", async () => {
  const WT = "/w/sample-app.worktrees/PROJ-7";
  const ws = (resume) => ({ issueKey: "PROJ-7", resume });
  // A tampered response: session and worktree both point at /tmp/evil.
  const evil = harness({ ticket_workspace: ws({ session: { ...SESSION, cwd: "/tmp/evil" }, worktreeDir: "/tmp/evil" }) });
  assert.equal(await main(["resume", "PROJ-7"], evil.deps), 1);
  assert.deepEqual(evil.calls.filter((c) => c[0] === "claude"), []);
  assert.match(evil.err.join(""), /Nothing to resume for PROJ-7/);
  // No allow-list dep at all: nothing is allowed.
  const noDep = harness({ ticket_workspace: ws({ session: { ...SESSION, cwd: WT }, worktreeDir: WT }) }, { cwdAllowed: undefined });
  assert.equal(await main(["resume", "PROJ-7"], noDep.deps), 1);
  // An invalid session (bad id) falls back to the worktree, naming the typed key.
  const badSession = harness({ ticket_workspace: ws({ session: { id: "x", cwd: WT, permissionMode: "plan" }, worktreeDir: WT }) });
  assert.equal(await main(["resume", "proj-7"], badSession.deps), 0);
  assert.deepEqual(badSession.calls[1], ["claude", [], WT]);
  assert.match(badSession.out.join(""), /No resumable Claude session for PROJ-7 /);
  // claude's exit code is the command's.
  const failing = harness({ ticket_workspace: ws({ session: { ...SESSION, cwd: WT }, worktreeDir: WT }) }, { runClaude: async () => 3 });
  assert.equal(await main(["resume", "PROJ-7"], failing.deps), 3);
});

test("resume with something that isn't an issue key is a job id (get_job); with nothing it is a usage error", async () => {
  const h = harness({ get_job: { id: "j", featureId: "analyze-issue", status: "failed", data: {} } });
  assert.equal(await main(["resume", "PROJ-0"], h.deps), 1);
  assert.deepEqual(h.calls[0], ["get_job", { jobId: "PROJ-0" }]);
  const none = harness({});
  assert.equal(await main(["resume"], none.deps), 2);
  assert.deepEqual(none.calls, []);
});
