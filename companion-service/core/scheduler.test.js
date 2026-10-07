// core/scheduler.ts from the build (npm run build first), with a fake clock
// and fake timers — nothing here waits on real time — and its state file in
// a temp dir.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "scheduler-home-"));
const { Scheduler } = require("../dist/core/scheduler.js");

const MIN = 60 * 1000;
const TUE_0800 = new Date(2026, 8, 29, 8, 0).getTime();

function fakeClock(start = TUE_0800) {
  let now = start;
  const timers = [];
  return {
    clock: {
      now: () => now,
      setTimer: (fn, ms) => {
        const t = { fn, at: now + ms, cleared: false };
        timers.push(t);
        return t;
      },
      clearTimer: (t) => {
        t.cleared = true;
      },
    },
    advance: (ms) => (now += ms),
    timers,
  };
}

function fakeHistory() {
  const events = [];
  return { events, history: { recordEvent: (e) => events.push(e) } };
}

function make({ config = {}, tasks = {}, statePath = null, isJobRunning } = {}) {
  const c = fakeClock();
  const h = fakeHistory();
  const logs = [];
  const s = new Scheduler({ config, tasks, statePath, history: h.history, clock: c.clock, isJobRunning, log: (m) => logs.push(m) });
  return { s, ...c, ...h, logs };
}

test("maintenance runs a minute after start; an enabled watcher at once; each run is an events row", async () => {
  const ran = [];
  const t = make({
    config: { similar: { enabled: false }, watchers: { conflicts: { enabled: true, intervalMinutes: 15 } } },
    tasks: {
      maintenance: async () => {
        ran.push("maintenance");
        return { pruned: 2 };
      },
      "watcher.conflicts": async () => {
        ran.push("conflicts");
        return { found: 1 };
      },
    },
  });
  assert.deepEqual(await t.s.tick(), ["watcher.conflicts"]);
  t.advance(MIN);
  assert.deepEqual(await t.s.tick(), ["maintenance"]);
  t.advance(5 * MIN);
  assert.deepEqual(await t.s.tick(), []);
  t.advance(10 * MIN);
  assert.deepEqual(await t.s.tick(), ["watcher.conflicts"]);
  assert.deepEqual(ran, ["conflicts", "maintenance", "conflicts"]);
  assert.equal(t.events.length, 3);
  const e = t.events[0];
  assert.match(e.jobId, /^sched:watcher\.conflicts:[0-9a-f-]{36}$/);
  assert.equal(e.featureId, "scheduler:watcher.conflicts");
  assert.equal(e.outcome, "completed");
  assert.deepEqual(e.metrics, { task: "watcher.conflicts", found: 1 });
  assert.equal(t.events[1].metrics.pruned, 2);
});

test("one task at a time: a tick while another is running does nothing", async () => {
  let release;
  const t = make({
    config: { watchers: { conflicts: { enabled: true } } },
    tasks: { "watcher.conflicts": () => new Promise((resolve) => (release = resolve)) },
  });
  const first = t.s.tick();
  assert.equal(t.s.status().running, "watcher.conflicts");
  assert.deepEqual(await t.s.tick(), []);
  release({});
  assert.deepEqual(await first, ["watcher.conflicts"]);
  assert.equal(t.s.status().running, null);
});

test("a failing task is recorded as failed with a redacted error, and needs-login shows in status", async () => {
  const t = make({
    config: { watchers: { conflicts: { enabled: true }, assignedBugs: { enabled: true } } },
    tasks: {
      "watcher.conflicts": async () => {
        throw new Error("GET https://u:secret@bb/x failed: Bearer abc123");
      },
      "watcher.assignedBugs": async () => ({ outcome: "failed", needsLogin: true, error: "Jira: no session" }),
    },
  });
  assert.deepEqual(await t.s.tick(), ["watcher.conflicts", "watcher.assignedBugs"]);
  assert.equal(t.events[0].outcome, "failed");
  assert.equal(t.events[0].metrics.error, "GET https://***@bb/x failed: Bearer ***");
  assert.equal(t.events[1].metrics.needsLogin, true);
  const status = t.s.status();
  assert.equal(status.watchers.conflicts.last.outcome, "failed");
  assert.equal(status.watchers.assignedBugs.last.needsLogin, true);
  assert.equal(status.watchers.reviewRequests.last, null);
  assert.ok(t.logs.some((m) => /watcher\.conflicts failed/.test(m)));
});

test("requestClaudeRun: the daily budget, one background run at a time, none in quiet hours", async () => {
  let running = true;
  const t = make({ config: { budget: { claudeRunsPerDay: 2 } }, isJobRunning: () => running });
  const first = t.s.requestClaudeRun();
  assert.equal(first.granted, true);
  assert.deepEqual(first.usage, { day: "2026-09-29", used: 1, limit: 2, remaining: 1 });
  t.s.trackBackgroundJob("job-1");
  assert.equal(t.s.requestClaudeRun().reason, "busy");
  running = false;
  assert.equal(t.s.requestClaudeRun().granted, true);
  assert.equal(t.s.requestClaudeRun().reason, "budget");
  assert.deepEqual(t.s.status().budget, { day: "2026-09-29", used: 2, limit: 2, remaining: 0 });

  const quiet = make({ config: { scheduler: { quietHours: { start: "07:00", end: "09:00" } } } });
  assert.equal(quiet.s.requestClaudeRun().reason, "quiet");
  assert.equal(quiet.s.status().quiet, true);
  const zero = make({ config: { budget: { claudeRunsPerDay: 0 } } });
  assert.equal(zero.s.requestClaudeRun().reason, "budget");
});

test("settings apply live: a watcher turned on later runs on the next tick", async () => {
  const config = {};
  let runs = 0;
  const t = make({ config, tasks: { "watcher.reviewRequests": async () => void runs++ } });
  assert.deepEqual(await t.s.tick(), []);
  config.watchers = { reviewRequests: { enabled: true } };
  assert.deepEqual(await t.s.tick(), ["watcher.reviewRequests"]);
  assert.equal(runs, 1);
});

test("the budget and what watchers have seen survive a restart; the file is 0600", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "scheduler-state-"));
  try {
    const statePath = path.join(dir, "scheduler.json");
    const tasks = {
      "watcher.conflicts": async (ctx) => {
        ctx.setSeen("conflicts", { ...ctx.seen("conflicts"), "bitbucket:CI/sample-app#1": "a:b" });
        assert.equal(ctx.requestClaudeRun().granted, true);
      },
    };
    const config = { watchers: { conflicts: { enabled: true } } };
    const first = make({ config, tasks, statePath });
    await first.s.tick();
    assert.equal(fs.statSync(statePath).mode & 0o777, 0o600);
    const again = make({ config, tasks: {}, statePath });
    assert.equal(again.s.status().budget.used, 1);
    const saved = JSON.parse(fs.readFileSync(statePath, "utf8"));
    assert.deepEqual(saved.seen.conflicts, { "bitbucket:CI/sample-app#1": "a:b" });
    assert.equal(typeof saved.lastRun["watcher.conflicts"], "number");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("the digest runs once on a weekday after its time", async () => {
  let digests = 0;
  const t = make({ config: { digest: { enabled: true, time: "08:30" } }, tasks: { digest: async () => void digests++ } });
  t.advance(29 * MIN);
  await t.s.tick();
  assert.equal(digests, 0);
  t.advance(MIN);
  await t.s.tick();
  await t.s.tick();
  assert.equal(digests, 1);
});

test("start arms a timer from the injected clock; firing it ticks and re-arms; stop clears it", async () => {
  let runs = 0;
  const t = make({ config: { watchers: { conflicts: { enabled: true } } }, tasks: { "watcher.conflicts": async () => void runs++ } });
  t.s.start();
  assert.equal(t.timers.length, 1);
  assert.equal(t.timers[0].at - TUE_0800, 1000, "something is due now: wake in a second");
  t.timers[0].fn();
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(runs, 1);
  assert.equal(t.timers.length, 2, "re-armed after the tick");
  t.s.stop();
  assert.equal(t.timers[1].cleared, true);
});

test("quiet hours: an enabled watcher does not make the loop spin; one timer sleeps to the next wake", async () => {
  const t = make({
    config: {
      watchers: { conflicts: { enabled: true } },
      scheduler: { quietHours: { start: "07:00", end: "09:00" } },
    },
    tasks: { "watcher.conflicts": async () => {} },
  });
  t.s.start();
  assert.equal(t.timers.length, 1);
  assert.ok(t.timers[0].at - TUE_0800 >= MIN, "not the one-second busy-wait of a due watcher");
  assert.deepEqual(await t.s.tick(), [], "the watcher is held during quiet hours");
  t.s.stop();
});

test("stop() during a task: the rest of that tick's tasks do not start", async () => {
  let release;
  const ran = [];
  const t = make({
    config: { watchers: { conflicts: { enabled: true }, assignedBugs: { enabled: true } } },
    tasks: {
      "watcher.conflicts": () => new Promise((resolve) => (release = resolve)),
      "watcher.assignedBugs": async () => void ran.push("assignedBugs"),
    },
  });
  const tick = t.s.tick();
  t.s.stop();
  release({});
  assert.deepEqual(await tick, ["watcher.conflicts"]);
  assert.deepEqual(ran, []);
  t.s.start();
  assert.deepEqual(await t.s.tick(), ["watcher.assignedBugs"], "start() lets ticks run tasks again");
  t.s.stop();
});

test("string metrics are redacted and clipped before they are recorded", async () => {
  const t = make({
    config: { watchers: { conflicts: { enabled: true } } },
    tasks: { "watcher.conflicts": async () => ({ note: "GET https://u:secret@bb/x with Bearer abc123", count: 3, long: "x".repeat(900) }) },
  });
  await t.s.tick();
  const m = t.events[0].metrics;
  assert.equal(m.note, "GET https://***@bb/x with Bearer ***");
  assert.equal(m.count, 3);
  assert.equal(m.long.length, 500);
});

test("a state file that can't be written is logged, not thrown, and the run still counts", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "scheduler-nowrite-"));
  try {
    const blocker = path.join(dir, "file");
    fs.writeFileSync(blocker, "x");
    let runs = 0;
    const t = make({ config: { watchers: { conflicts: { enabled: true } } }, tasks: { "watcher.conflicts": async () => void runs++ }, statePath: path.join(blocker, "sub", "scheduler.json") });
    assert.deepEqual(await t.s.tick(), ["watcher.conflicts"]);
    assert.equal(runs, 1);
    assert.ok(t.logs.some((m) => /couldn't save the scheduler state/.test(m)));
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("the Claude budget starts over at local midnight", () => {
  const t = make({ config: { budget: { claudeRunsPerDay: 1 } }, tasks: {} });
  assert.equal(t.s.requestClaudeRun().granted, true);
  assert.equal(t.s.requestClaudeRun().reason, "budget");
  t.advance(16 * 60 * MIN + 1); // 08:00 + 16h -> just past midnight
  const next = t.s.requestClaudeRun();
  assert.equal(next.granted, true);
  assert.equal(next.usage.used, 1);
});

test("stop() then start() leaves exactly one live timer", () => {
  const t = make({ config: { watchers: { conflicts: { enabled: true } } }, tasks: { "watcher.conflicts": async () => {} } });
  t.s.start();
  t.s.stop();
  t.s.start();
  assert.equal(t.timers.filter((x) => !x.cleared).length, 1);
  t.s.stop();
  assert.equal(t.timers.filter((x) => !x.cleared).length, 0);
});

test("refundClaudeRun gives back one run today, never below zero, and not from a past day", () => {
  const t = make({ config: { budget: { claudeRunsPerDay: 2 } }, tasks: {} });
  t.s.requestClaudeRun();
  t.s.requestClaudeRun();
  assert.equal(t.s.requestClaudeRun().reason, "budget");
  t.s.refundClaudeRun();
  assert.equal(t.s.status().budget.used, 1);
  assert.equal(t.s.requestClaudeRun().granted, true);
  t.s.refundClaudeRun();
  t.s.refundClaudeRun();
  t.s.refundClaudeRun();
  assert.equal(t.s.status().budget.used, 0);
  t.s.requestClaudeRun();
  t.advance(17 * 60 * MIN);
  t.s.refundClaudeRun();
  assert.equal(t.s.status().budget.used, 0, "the new day starts at zero anyway");
});

test("a watcher task's refund reaches the scheduler's budget", async () => {
  const t = make({
    config: { watchers: { conflicts: { enabled: true } } },
    tasks: {
      "watcher.conflicts": async (ctx) => {
        assert.equal(ctx.requestClaudeRun().granted, true);
        ctx.refundClaudeRun();
      },
    },
  });
  await t.s.tick();
  assert.equal(t.s.status().budget.used, 0);
});

test("with no history.embed registered the loop sleeps a minute at a time, never a second", async () => {
  const t = make({ tasks: { maintenance: async () => ({}) } });
  t.advance(60 * MIN);
  assert.deepEqual(await t.s.tick(), ["maintenance"]);
  t.s.start();
  const armed = t.timers.filter((x) => !x.cleared).pop();
  assert.equal(armed.at - t.clock.now(), MIN);
  t.s.stop();
});

test("a registered history.embed that fails is retried every 30 minutes, not every second", async () => {
  const t = make({ tasks: { maintenance: async () => ({}), "history.embed": async () => ({ outcome: "failed", error: "boom" }) } });
  t.advance(3 * MIN);
  assert.deepEqual(await t.s.tick(), ["maintenance", "history.embed"]);
  t.s.start();
  assert.equal(t.timers.filter((x) => !x.cleared).pop().at - t.clock.now(), MIN);
  t.s.stop();
  t.advance(10 * MIN);
  assert.deepEqual(await t.s.tick(), []);
});
