const test = require("node:test");
const assert = require("node:assert/strict");
const s = require("./schedule.js");

// Local-time instants: Tuesday 29 Sep 2026 and Sunday 27 Sep 2026.
const at = (day, h, m = 0) => new Date(2026, 8, day, h, m).getTime();
const TUE_0800 = at(29, 8);
const MIN = 60 * 1000;
// Most tests here are about the watchers and the digest: keep the Phase 8
// embed task (on by default) out of their way.
const NO_EMBED = { similar: { enabled: false } };

test("parseHm, localDayKey and isWeekday read local time", () => {
  assert.equal(s.parseHm("08:30"), 510);
  assert.equal(s.parseHm(" 23:59 "), 1439);
  for (const bad of ["8:30", "24:00", "12:60", "", null, 830]) assert.equal(s.parseHm(bad), null);
  assert.equal(s.localDayKey(TUE_0800), "2026-09-29");
  assert.equal(s.isWeekday(TUE_0800), true);
  assert.equal(s.isWeekday(at(27, 8)), false);
});

test("quiet hours: off when unset, malformed or empty; wrap past midnight", () => {
  assert.equal(s.quietHoursFrom({}), null);
  assert.equal(s.quietHoursFrom({ scheduler: { quietHours: { start: "7pm", end: "08:00" } } }), null);
  assert.equal(s.quietHoursFrom({ scheduler: { quietHours: { start: "08:00", end: "08:00" } } }), null);
  const night = s.quietHoursFrom({ scheduler: { quietHours: { start: "19:00", end: "08:00" } } });
  assert.deepEqual(night, { start: 1140, end: 480 });
  assert.equal(s.inQuietHours(at(29, 22), night), true);
  assert.equal(s.inQuietHours(at(29, 7, 59), night), true);
  assert.equal(s.inQuietHours(TUE_0800, night), false);
  const lunch = { start: 720, end: 780 };
  assert.equal(s.inQuietHours(at(29, 12, 30), lunch), true);
  assert.equal(s.inQuietHours(at(29, 13), lunch), false);
  assert.equal(s.inQuietHours(TUE_0800, null), false);
});

test("watchers are off by default and intervals fall back to the defaults when out of range", () => {
  assert.deepEqual(s.watcherSettings({}), {
    conflicts: { enabled: false, intervalMinutes: 15 },
    assignedBugs: { enabled: false, intervalMinutes: 60 },
    reviewRequests: { enabled: false, intervalMinutes: 30 },
  });
  const w = s.watcherSettings({ watchers: { conflicts: { enabled: true, intervalMinutes: 1 }, assignedBugs: { enabled: "yes", intervalMinutes: 0 } } });
  assert.deepEqual(w.conflicts, { enabled: true, intervalMinutes: 1 });
  assert.deepEqual(w.assignedBugs, { enabled: false, intervalMinutes: 60 });
  assert.equal(s.budgetLimit({}), 5);
  assert.equal(s.budgetLimit({ budget: { claudeRunsPerDay: 0 } }), 0);
  assert.equal(s.budgetLimit({ budget: { claudeRunsPerDay: 99 } }), 5);
  assert.deepEqual(s.digestSettings({}), { enabled: false, time: "08:30" });
  assert.deepEqual(s.digestSettings({ digest: { enabled: true, time: "09:15" } }), { enabled: true, time: "09:15" });
  assert.equal(s.notifyMinIntervalMs({}), 30 * MIN);
  assert.equal(s.notifyMinIntervalMs({ notify: { minIntervalMinutes: 0 } }), 0);
});

test("normalizeState keeps valid fields, drops junk and never carries maintenance over", () => {
  const fresh = s.normalizeState(null);
  assert.deepEqual(fresh, {
    version: 1,
    lastRun: {},
    budget: { day: "", used: 0 },
    lastDigestDay: null,
    seen: { conflicts: {}, assignedBugs: {}, reviewRequests: {} },
  });
  const loaded = s.normalizeState({
    lastRun: { maintenance: 5, "watcher.conflicts": 7, bogus: 1 },
    budget: { day: "2026-09-29", used: -1 },
    lastDigestDay: "2026-09-28",
    seen: { conflicts: { "CI/sample-app#1": "a:b", bad: 5 } },
  });
  assert.deepEqual(loaded.lastRun, { "watcher.conflicts": 7 });
  assert.deepEqual(loaded.budget, { day: "", used: 0 });
  assert.equal(loaded.lastDigestDay, "2026-09-28");
  assert.deepEqual(loaded.seen.conflicts, { "CI/sample-app#1": "a:b" });
});

test("maintenance runs a minute after start and then daily; an enabled watcher runs at once, then on its interval", () => {
  const config = { ...NO_EMBED, watchers: { conflicts: { enabled: true, intervalMinutes: 15 } } };
  const state = s.normalizeState(null);
  const startedAt = TUE_0800;
  assert.deepEqual(s.dueTasks({ config, state, now: startedAt, startedAt }), ["watcher.conflicts"]);
  assert.deepEqual(s.dueTasks({ config, state, now: startedAt + MIN, startedAt }), ["maintenance", "watcher.conflicts"]);
  const ran = { ...state, lastRun: { maintenance: startedAt + MIN, "watcher.conflicts": startedAt + MIN } };
  assert.deepEqual(s.dueTasks({ config, state: ran, now: startedAt + 10 * MIN, startedAt }), []);
  assert.deepEqual(s.dueTasks({ config, state: ran, now: startedAt + 16 * MIN, startedAt }), ["watcher.conflicts"]);
  assert.deepEqual(s.dueTasks({ config, state: ran, now: startedAt + 24 * 60 * MIN + MIN, startedAt }), ["maintenance", "watcher.conflicts"]);
});

test("quiet hours skip the watchers but not maintenance", () => {
  const config = {
    watchers: { conflicts: { enabled: true } },
    scheduler: { quietHours: { start: "19:00", end: "08:00" } },
  };
  const state = s.normalizeState(null);
  const night = at(29, 23);
  assert.deepEqual(s.dueTasks({ config, state, now: night + MIN, startedAt: night }), ["maintenance"]);
});

test("the digest is due once per weekday after its time", () => {
  const config = { ...NO_EMBED, digest: { enabled: true, time: "08:30" } };
  const state = s.normalizeState(null);
  const ctx = (now, st = state) => ({ config, state: { ...st, lastRun: { maintenance: now } }, now, startedAt: now - 2 * MIN });
  assert.deepEqual(s.dueTasks(ctx(at(29, 8, 29))), []);
  assert.deepEqual(s.dueTasks(ctx(at(29, 8, 30))), ["digest"]);
  assert.deepEqual(s.dueTasks(ctx(at(29, 17), { ...state, lastDigestDay: "2026-09-29" })), []);
  assert.deepEqual(s.dueTasks(ctx(at(27, 9))), [], "not on a Sunday");
  assert.deepEqual(s.dueTasks({ config: NO_EMBED, state, now: at(29, 9), startedAt: at(29, 9) }), []);
});

test("nextWakeDelay sleeps until the next due task, at most a minute and at least a second", () => {
  const state = { ...s.normalizeState(null), lastRun: { maintenance: TUE_0800, "watcher.conflicts": TUE_0800 } };
  const config = { ...NO_EMBED, watchers: { conflicts: { enabled: true, intervalMinutes: 1 } } };
  assert.equal(s.nextWakeDelay({ config, state, now: TUE_0800 + 30 * 1000, startedAt: TUE_0800 }), 30 * 1000);
  assert.equal(s.nextWakeDelay({ config: NO_EMBED, state, now: TUE_0800, startedAt: TUE_0800 }), MIN);
  assert.equal(s.nextWakeDelay({ config, state, now: TUE_0800 + 5 * MIN, startedAt: TUE_0800 }), 1000);
});

test("grantClaudeRun counts runs per local day and refuses when busy, quiet or spent", () => {
  let state = s.normalizeState(null);
  const r1 = s.grantClaudeRun({ state, now: TUE_0800, limit: 2, busy: false, quiet: false });
  assert.equal(r1.granted, true);
  assert.deepEqual(r1.usage, { day: "2026-09-29", used: 1, limit: 2, remaining: 1 });
  state = r1.state;
  assert.equal(s.grantClaudeRun({ state, now: TUE_0800, limit: 2, busy: true, quiet: false }).reason, "busy");
  assert.equal(s.grantClaudeRun({ state, now: TUE_0800, limit: 2, busy: false, quiet: true }).reason, "quiet");
  state = s.grantClaudeRun({ state, now: TUE_0800, limit: 2, busy: false, quiet: false }).state;
  const spent = s.grantClaudeRun({ state, now: TUE_0800, limit: 2, busy: false, quiet: false });
  assert.equal(spent.granted, false);
  assert.equal(spent.reason, "budget");
  assert.equal(spent.state, state, "a refusal leaves the state as it was");
  assert.equal(s.grantClaudeRun({ state, now: at(30, 8), limit: 2, busy: false, quiet: false }).granted, true, "a new day starts at zero");
  assert.equal(s.grantClaudeRun({ state: s.normalizeState(null), now: TUE_0800, limit: 0, busy: false, quiet: false }).reason, "budget");
});

test("nextWakeDelay does not spin through quiet hours with a watcher enabled", () => {
  const config = {
    ...NO_EMBED,
    watchers: { conflicts: { enabled: true, intervalMinutes: 1 } },
    scheduler: { quietHours: { start: "19:00", end: "08:00" } },
  };
  const night = at(29, 23);
  const state = { ...s.normalizeState(null, night), lastRun: { maintenance: night, "watcher.conflicts": night - 10 * MIN } };
  assert.equal(s.nextWakeDelay({ config, state, now: night, startedAt: night - 60 * MIN }), MIN, "no 1 s spin");
  assert.equal(s.nextWakeDelay({ config, state, now: at(29, 7, 59) + 30 * 1000, startedAt: night - 600 * MIN }), 30 * 1000, "wakes at the window end");
  assert.equal(s.nextWakeDelay({ config, state, now: at(30, 8), startedAt: night - 600 * MIN }), 1000, "after the window the watcher is due");
});

test("nextWakeDelay: a digest already done today is not a past wake time", () => {
  const config = { ...NO_EMBED, digest: { enabled: true, time: "08:30" } };
  const now = at(29, 17);
  const state = { ...s.normalizeState(null, now), lastDigestDay: "2026-09-29", lastRun: { maintenance: now } };
  assert.equal(s.nextWakeDelay({ config, state, now, startedAt: now - 600 * MIN }), MIN);
});

test("quiet hours boundaries: start inclusive, end exclusive, midnight and 23:59", () => {
  const night = { start: 1140, end: 480 };
  assert.equal(s.inQuietHours(at(29, 19), night), true, "exactly start");
  assert.equal(s.inQuietHours(at(29, 18, 59), night), false);
  assert.equal(s.inQuietHours(at(29, 0), night), true, "00:00");
  assert.equal(s.inQuietHours(at(29, 23, 59), night), true, "23:59");
  assert.equal(s.inQuietHours(at(29, 8), night), false, "exactly end");
  const early = s.quietHoursFrom({ scheduler: { quietHours: { start: "00:00", end: "06:00" } } });
  assert.deepEqual(early, { start: 0, end: 360 });
  assert.equal(s.inQuietHours(at(29, 0), early), true);
  assert.equal(s.inQuietHours(at(29, 5, 59), early), true);
  assert.equal(s.inQuietHours(at(29, 6), early), false);
  assert.equal(s.inQuietHours(at(29, 23, 59), early), false);
});

test("normalizeState clamps a future lastRun and resets a future budget day", () => {
  const now = TUE_0800;
  const st = s.normalizeState(
    { lastRun: { "watcher.conflicts": now + 5 * 60 * MIN }, budget: { day: "2026-10-05", used: 0 }, lastDigestDay: "2026-10-05" },
    now
  );
  assert.equal(st.lastRun["watcher.conflicts"], now);
  assert.deepEqual(st.budget, { day: "", used: 0 });
  assert.equal(st.lastDigestDay, null);
  const g = s.grantClaudeRun({ state: st, now, limit: 1, busy: false, quiet: false });
  assert.equal(g.granted, true);
  assert.equal(s.grantClaudeRun({ state: g.state, now, limit: 1, busy: false, quiet: false }).reason, "budget");
});

test("history.embed runs two minutes after start and then every 30 minutes, also in quiet hours, unless turned off", () => {
  const state = s.normalizeState(null);
  const quiet = { scheduler: { quietHours: { start: "19:00", end: "08:00" } } };
  const night = at(29, 23);
  assert.deepEqual(s.dueTasks({ config: {}, state, now: TUE_0800 + MIN, startedAt: TUE_0800 }), ["maintenance"]);
  assert.deepEqual(s.dueTasks({ config: {}, state, now: TUE_0800 + 2 * MIN, startedAt: TUE_0800 }), ["maintenance", "history.embed"]);
  const ran = { ...state, lastRun: { maintenance: TUE_0800 + MIN, "history.embed": TUE_0800 + 2 * MIN } };
  assert.deepEqual(s.dueTasks({ config: {}, state: ran, now: TUE_0800 + 31 * MIN, startedAt: TUE_0800 }), []);
  assert.deepEqual(s.dueTasks({ config: {}, state: ran, now: TUE_0800 + 32 * MIN, startedAt: TUE_0800 }), ["history.embed"]);
  assert.deepEqual(s.dueTasks({ config: quiet, state, now: night + 2 * MIN, startedAt: night }), ["maintenance", "history.embed"]);
  assert.deepEqual(s.dueTasks({ config: NO_EMBED, state, now: TUE_0800 + 2 * MIN, startedAt: TUE_0800 }), ["maintenance"]);
  assert.equal(s.TASKS[s.TASKS.length - 1], "history.embed", "it runs last in a tick");
  assert.equal(s.normalizeState({ lastRun: { "history.embed": 5 } }, TUE_0800).lastRun["history.embed"], 5, "its last run survives a restart");
});

test("wake computation only counts tasks that are registered: no history.embed means the normal wake, never 1 s", () => {
  const state = { ...s.normalizeState(null), lastRun: { maintenance: TUE_0800 } };
  const ctx = { config: {}, state, now: TUE_0800 + 60 * MIN, startedAt: TUE_0800 };
  assert.equal(s.nextWakeDelay({ ...ctx, registered: ["maintenance"] }), MIN);
  assert.deepEqual(s.dueTasks({ ...ctx, registered: ["maintenance"] }), []);
  assert.equal(s.nextWakeDelay(ctx), 1000, "without a registered list every task counts (the old behaviour)");
});

test("a registered history.embed is due at start + 2 min, then every 30 min", () => {
  const registered = ["maintenance", "history.embed"];
  const base = { config: {}, startedAt: TUE_0800, registered };
  let state = { ...s.normalizeState(null), lastRun: { maintenance: TUE_0800 } };
  assert.deepEqual(s.dueTasks({ ...base, state, now: TUE_0800 + MIN }), []);
  assert.equal(s.nextWakeDelay({ ...base, state, now: TUE_0800 + MIN }), MIN);
  assert.deepEqual(s.dueTasks({ ...base, state, now: TUE_0800 + 2 * MIN }), ["history.embed"]);
  state = { ...state, lastRun: { ...state.lastRun, "history.embed": TUE_0800 + 2 * MIN } };
  assert.deepEqual(s.dueTasks({ ...base, state, now: TUE_0800 + 31 * MIN }), []);
  assert.deepEqual(s.dueTasks({ ...base, state, now: TUE_0800 + 32 * MIN }), ["history.embed"]);
  assert.equal(s.nextWakeDelay({ ...base, state, now: TUE_0800 + 3 * MIN }), MIN);
});

test("a registered task that keeps failing doesn't spin: its run time is recorded, so the wake is not 1 s", () => {
  const registered = ["maintenance", "history.embed"];
  const state = { ...s.normalizeState(null), lastRun: { maintenance: TUE_0800, "history.embed": TUE_0800 + 2 * MIN } };
  assert.equal(s.nextWakeDelay({ config: {}, state, now: TUE_0800 + 3 * MIN, startedAt: TUE_0800, registered }), MIN);
});
