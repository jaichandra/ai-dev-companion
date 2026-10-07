// Pure rules for the background scheduler (core/scheduler.ts): which tasks
// are due, quiet hours, the daily Claude-run budget and the weekday digest
// time. No timers and no I/O — every function takes `now` (epoch ms, read
// in local time), so tests drive it with plain numbers.
const { similarSettings } = require("./similar.js");
const WATCHERS = ["conflicts", "assignedBugs", "reviewRequests"];
const WATCHER_DEFAULT_MINUTES = { conflicts: 15, assignedBugs: 60, reviewRequests: 30 };
const MIN_INTERVAL_MINUTES = 1;
const MAX_INTERVAL_MINUTES = 1440;
const DEFAULT_CLAUDE_RUNS_PER_DAY = 5;
const MAX_CLAUDE_RUNS_PER_DAY = 50;
const DEFAULT_DIGEST_TIME = "08:30";
const DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES = 30;
const MINUTE_MS = 60 * 1000;
const MAINTENANCE_INTERVAL_MS = 24 * 60 * MINUTE_MS;
const MAINTENANCE_FIRST_DELAY_MS = MINUTE_MS;
// history.embed (Phase 8): embeds new history items for similar-item search.
const EMBED_INTERVAL_MS = 30 * MINUTE_MS;
const EMBED_FIRST_DELAY_MS = 2 * MINUTE_MS;
const MAX_WAKE_MS = MINUTE_MS;
const MIN_WAKE_MS = 1000;
const HM_RE = /^([01]\d|2[0-3]):([0-5]\d)$/;

/** The scheduler's task names, in the order a tick runs them. */
const TASKS = ["maintenance", ...WATCHERS.map((w) => `watcher.${w}`), "digest", "history.embed"];

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);

/** "08:30" -> 510 (minutes after midnight); null for anything else. */
function parseHm(text) {
  const m = typeof text === "string" ? HM_RE.exec(text.trim()) : null;
  return m ? Number(m[1]) * 60 + Number(m[2]) : null;
}

function minutesOfDay(now) {
  const d = new Date(now);
  return d.getHours() * 60 + d.getMinutes();
}

/** Local calendar day, "2026-09-29". */
function localDayKey(now) {
  const d = new Date(now);
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function isWeekday(now) {
  const day = new Date(now).getDay();
  return day >= 1 && day <= 5;
}

/** `scheduler.quietHours` as `{start, end}` minutes, or null when unset or
 * malformed (a broken setting never silences anything). Start == end is off. */
function quietHoursFrom(config) {
  const q = config && config.scheduler && config.scheduler.quietHours;
  if (!isObject(q)) return null;
  const start = parseHm(q.start);
  const end = parseHm(q.end);
  if (start === null || end === null || start === end) return null;
  return { start, end };
}

/** Whether `now` falls in the quiet hours; they may wrap past midnight. */
function inQuietHours(now, quiet) {
  if (!quiet) return false;
  const m = minutesOfDay(now);
  return quiet.start < quiet.end ? m >= quiet.start && m < quiet.end : m >= quiet.start || m < quiet.end;
}

function clampInt(value, min, max, fallback) {
  return Number.isInteger(value) && value >= min && value <= max ? value : fallback;
}

/** Every watcher's effective `{enabled, intervalMinutes}`; off unless opted in. */
function watcherSettings(config) {
  const saved = (config && config.watchers) || {};
  const out = {};
  for (const name of WATCHERS) {
    const w = isObject(saved[name]) ? saved[name] : {};
    out[name] = {
      enabled: w.enabled === true,
      intervalMinutes: clampInt(w.intervalMinutes, MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES, WATCHER_DEFAULT_MINUTES[name]),
    };
  }
  return out;
}

function budgetLimit(config) {
  const b = config && config.budget;
  return clampInt(b && b.claudeRunsPerDay, 0, MAX_CLAUDE_RUNS_PER_DAY, DEFAULT_CLAUDE_RUNS_PER_DAY);
}

function digestSettings(config) {
  const d = (config && config.digest) || {};
  return { enabled: d.enabled === true, time: parseHm(d.time) === null ? DEFAULT_DIGEST_TIME : d.time.trim() };
}

function notifyMinIntervalMs(config) {
  const n = config && config.notify;
  return clampInt(n && n.minIntervalMinutes, 0, 1440, DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES) * MINUTE_MS;
}

/** A fresh scheduler state, or a loaded one with anything malformed dropped. */
function normalizeState(raw, now = Date.now()) {
  const s = isObject(raw) ? raw : {};
  const today = localDayKey(now);
  const lastRun = {};
  if (isObject(s.lastRun)) {
    for (const task of TASKS) if (Number.isFinite(s.lastRun[task])) lastRun[task] = Math.min(s.lastRun[task], now);
  }
  // Maintenance always runs a minute after start (like the old startup
  // prune), so its last run is never carried across a restart.
  delete lastRun.maintenance;
  const budget =
    isObject(s.budget) && typeof s.budget.day === "string" && s.budget.day <= today && Number.isInteger(s.budget.used) && s.budget.used >= 0
      ? { day: s.budget.day, used: s.budget.used }
      : { day: "", used: 0 };
  const seen = {};
  for (const name of WATCHERS) {
    const src = isObject(s.seen) && isObject(s.seen[name]) ? s.seen[name] : {};
    seen[name] = {};
    for (const [k, v] of Object.entries(src).slice(0, 500)) if (typeof v === "string" && v.length <= 200) seen[name][k] = v;
  }
  return {
    version: 1,
    lastRun,
    budget,
    lastDigestDay: typeof s.lastDigestDay === "string" && s.lastDigestDay <= today ? s.lastDigestDay : null,
    seen,
  };
}

/** When `task` is next due (epoch ms), or null when it isn't scheduled at all. */
function nextDueAt(task, { config, state, now, startedAt }) {
  const last = state.lastRun[task];
  if (task === "maintenance") {
    return last === undefined ? startedAt + MAINTENANCE_FIRST_DELAY_MS : last + MAINTENANCE_INTERVAL_MS;
  }
  if (task.startsWith("watcher.")) {
    const w = watcherSettings(config)[task.slice("watcher.".length)];
    if (!w || !w.enabled) return null;
    return last === undefined ? now : last + w.intervalMinutes * MINUTE_MS;
  }
  if (task === "history.embed") {
    // On unless similar items are turned off; it only calls the LLM proxy
    // when that is ready, and quiet hours don't stop it (it notifies no one).
    if (!similarSettings(config).enabled) return null;
    return last === undefined ? startedAt + EMBED_FIRST_DELAY_MS : last + EMBED_INTERVAL_MS;
  }
  if (task === "digest") {
    const d = digestSettings(config);
    if (!d.enabled) return null;
    const at = new Date(now);
    at.setHours(0, parseHm(d.time), 0, 0);
    const today = localDayKey(now);
    if (isWeekday(now) && state.lastDigestDay !== today) return at.getTime();
    // Tomorrow at the same time; the tick re-checks the weekday then.
    at.setDate(at.getDate() + 1);
    return at.getTime();
  }
  return null;
}

/** The tasks that can run: all of them, or (when the caller passes
 * `ctx.registered`) only those it has a function for — an unregistered task
 * must not keep the loop waking every second. */
function runnableTasks(ctx) {
  return Array.isArray(ctx.registered) ? TASKS.filter((t) => ctx.registered.includes(t)) : TASKS;
}

/** The tasks due at `now`, in TASKS order. Quiet hours skip the watchers
 * (nothing burns the budget or wakes anyone at night); maintenance and the
 * digest still run. */
function dueTasks(ctx) {
  const quiet = inQuietHours(ctx.now, quietHoursFrom(ctx.config));
  return runnableTasks(ctx).filter((task) => {
    if (quiet && task.startsWith("watcher.")) return false;
    const due = nextDueAt(task, ctx);
    return due !== null && due <= ctx.now;
  });
}

/** How long the scheduler may sleep before its next tick: until the next
 * due task, but never more than a minute, so a Settings change (a watcher
 * turned on, a new digest time) applies within a minute with no restart. */
function nextWakeDelay(ctx) {
  let soonest = ctx.now + MAX_WAKE_MS;
  const quiet = quietHoursFrom(ctx.config);
  const inQuiet = inQuietHours(ctx.now, quiet);
  if (inQuiet) {
    // Watchers can't run until the window ends: wake then, not every second.
    const end = new Date(ctx.now);
    end.setHours(0, quiet.end, 0, 0);
    if (end.getTime() <= ctx.now) end.setDate(end.getDate() + 1);
    if (end.getTime() < soonest) soonest = end.getTime();
  }
  for (const task of runnableTasks(ctx)) {
    if (inQuiet && task.startsWith("watcher.")) continue;
    const due = nextDueAt(task, ctx);
    if (due !== null && due < soonest) soonest = due;
  }
  return Math.max(MIN_WAKE_MS, Math.min(MAX_WAKE_MS, soonest - ctx.now));
}

/** Today's budget use. A new local day starts at zero. */
function budgetUsage(state, now, limit) {
  const day = localDayKey(now);
  const used = state.budget && state.budget.day === day ? state.budget.used : 0;
  return { day, used, limit, remaining: Math.max(0, limit - used) };
}

/**
 * May a background task start one more Claude run? Refused while another
 * background run is still going ("busy" — one at a time), in quiet hours
 * ("quiet"), or when today's budget is spent ("budget"). A grant returns
 * the state with the run counted; a refusal returns it unchanged.
 */
function grantClaudeRun({ state, now, limit, busy, quiet }) {
  const usage = budgetUsage(state, now, limit);
  if (busy) return { granted: false, reason: "busy", usage, state };
  if (quiet) return { granted: false, reason: "quiet", usage, state };
  if (usage.remaining <= 0) return { granted: false, reason: "budget", usage, state };
  const next = { ...state, budget: { day: usage.day, used: usage.used + 1 } };
  return { granted: true, usage: budgetUsage(next, now, limit), state: next };
}

/** Gives back one run counted today (a run that never started). Never below zero. */
function refundClaudeRun({ state, now }) {
  const day = localDayKey(now);
  if (!state.budget || state.budget.day !== day || state.budget.used <= 0) return state;
  return { ...state, budget: { day, used: state.budget.used - 1 } };
}

module.exports = {
  refundClaudeRun,
  WATCHERS,
  TASKS,
  WATCHER_DEFAULT_MINUTES,
  MIN_INTERVAL_MINUTES,
  MAX_INTERVAL_MINUTES,
  DEFAULT_CLAUDE_RUNS_PER_DAY,
  MAX_CLAUDE_RUNS_PER_DAY,
  DEFAULT_DIGEST_TIME,
  DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES,
  MAINTENANCE_INTERVAL_MS,
  EMBED_INTERVAL_MS,
  parseHm,
  localDayKey,
  isWeekday,
  quietHoursFrom,
  inQuietHours,
  watcherSettings,
  budgetLimit,
  digestSettings,
  notifyMinIntervalMs,
  normalizeState,
  nextDueAt,
  dueTasks,
  nextWakeDelay,
  budgetUsage,
  grantClaudeRun,
};
