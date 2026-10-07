// The background scheduler: one loop that runs the daily maintenance (job
// expiry and history pruning — the old Phase 4 timers), the opt-in
// watchers and the weekday digest, one task at a time. core/schedule.js
// decides what's due; this file keeps the state (the day's Claude budget,
// what each watcher has seen) and records every run as an `events` row.
// The clock and timers are injected so tests never wait on real time.
import * as fs from "fs";
import * as path from "path";
import { randomUUID } from "crypto";
import type { Config } from "../config";
import type { HistoryStore } from "./history";

interface SchedulerState {
  version: number;
  lastRun: Record<string, number>;
  budget: { day: string; used: number };
  lastDigestDay: string | null;
  seen: Record<string, Record<string, string>>;
}

export interface BudgetUsage {
  day: string;
  used: number;
  limit: number;
  remaining: number;
}

export interface ClaudeRunGrant {
  granted: boolean;
  reason?: "busy" | "quiet" | "budget";
  usage: BudgetUsage;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const schedule = require("./schedule.js") as {
  TASKS: string[];
  WATCHERS: string[];
  localDayKey(now: number): string;
  quietHoursFrom(config: Config): { start: number; end: number } | null;
  inQuietHours(now: number, quiet: { start: number; end: number } | null): boolean;
  watcherSettings(config: Config): Record<string, { enabled: boolean; intervalMinutes: number }>;
  budgetLimit(config: Config): number;
  normalizeState(raw: unknown): SchedulerState;
  dueTasks(ctx: { config: Config; state: SchedulerState; now: number; startedAt: number; registered?: string[] }): string[];
  nextWakeDelay(ctx: { config: Config; state: SchedulerState; now: number; startedAt: number; registered?: string[] }): number;
  budgetUsage(state: SchedulerState, now: number, limit: number): BudgetUsage;
  refundClaudeRun(opts: { state: SchedulerState; now: number }): SchedulerState;
  grantClaudeRun(opts: { state: SchedulerState; now: number; limit: number; busy: boolean; quiet: boolean }): ClaudeRunGrant & {
    state: SchedulerState;
  };
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { redactSecrets } = require("./history-record.js") as { redactSecrets(text: string): string };

/** What a task hands back: metrics for its events row, plus how it went. */
export interface TaskResult {
  outcome?: "completed" | "failed";
  /** True when the task couldn't sign in anywhere (no session, no token). */
  needsLogin?: boolean;
  error?: string;
  [metric: string]: unknown;
}

export interface TaskContext {
  now(): number;
  quiet: boolean;
  requestClaudeRun(): ClaudeRunGrant;
  /** The run just granted never started: give it back. */
  refundClaudeRun(): void;
  trackBackgroundJob(jobId: string): void;
  seen(watcher: string): Record<string, string>;
  setSeen(watcher: string, seen: Record<string, string>): void;
}

export type TaskFn = (ctx: TaskContext) => Promise<TaskResult | void>;

export interface Clock {
  now(): number;
  setTimer(fn: () => void, ms: number): unknown;
  clearTimer(handle: unknown): void;
}

export interface TaskStatus {
  at: number;
  outcome: "completed" | "failed";
  durationMs: number;
  needsLogin: boolean;
  error?: string;
}

const realClock: Clock = {
  now: () => Date.now(),
  setTimer: (fn, ms) => {
    const t = setTimeout(fn, ms);
    t.unref?.();
    return t;
  },
  clearTimer: (h) => clearTimeout(h as NodeJS.Timeout),
};

export class Scheduler {
  private state: SchedulerState;
  private readonly startedAt: number;
  private timer: unknown = null;
  private ticking = false;
  private stopped = true;
  /** stop() was called (and start() hasn't been since): a running tick starts no further task. */
  private halted = false;
  private running: string | null = null;
  private backgroundJobId: string | null = null;
  private readonly lastStatus: Record<string, TaskStatus> = {};
  private readonly clock: Clock;

  constructor(
    private readonly opts: {
      config: Config;
      tasks: Partial<Record<string, TaskFn>>;
      /** Where the state is kept (stateDir()/scheduler.json); null keeps it in memory. */
      statePath?: string | null;
      history?: HistoryStore;
      clock?: Clock;
      /** Whether a job id is still running (jobStore.get(id)?.status === "running"). */
      isJobRunning?: (jobId: string) => boolean;
      log?: (message: string) => void;
    },
  ) {
    this.clock = opts.clock || realClock;
    this.startedAt = this.clock.now();
    this.state = schedule.normalizeState(this.load());
  }

  private load(): unknown {
    if (!this.opts.statePath) return null;
    try {
      return JSON.parse(fs.readFileSync(this.opts.statePath, "utf8"));
    } catch {
      return null;
    }
  }

  private save(): void {
    const file = this.opts.statePath;
    if (!file) return;
    const tmp = path.join(path.dirname(file), `.${path.basename(file)}.${process.pid}.tmp`);
    try {
      fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
      fs.writeFileSync(tmp, JSON.stringify(this.state, null, 2) + "\n", { mode: 0o600 });
      fs.renameSync(tmp, file);
    } catch (err) {
      try {
        fs.rmSync(tmp, { force: true });
      } catch {
        // nothing to clean up (the folder itself could not be made)
      }
      this.log(`WARN - couldn't save the scheduler state: ${(err as Error).message}`);
    }
  }

  private log(message: string): void {
    (this.opts.log || ((m: string) => console.log(`[scheduler] ${m}`)))(message);
  }

  private ctx(now: number) {
    return { config: this.opts.config, state: this.state, now, startedAt: this.startedAt, registered: Object.keys(this.opts.tasks) };
  }

  quietNow(): boolean {
    return schedule.inQuietHours(this.clock.now(), schedule.quietHoursFrom(this.opts.config));
  }

  /** Starts the loop: a tick whenever something is due, at least once a minute. */
  start(): void {
    this.stopped = false;
    this.halted = false;
    this.arm(schedule.nextWakeDelay(this.ctx(this.clock.now())));
  }

  stop(): void {
    this.stopped = true;
    this.halted = true;
    if (this.timer !== null) this.clock.clearTimer(this.timer);
    this.timer = null;
  }

  private arm(ms: number): void {
    if (this.stopped) return;
    if (this.timer !== null) this.clock.clearTimer(this.timer);
    this.timer = this.clock.setTimer(() => {
      this.timer = null;
      void this.tick()
        .catch((err: Error) => this.log(`WARN - tick failed: ${err.message}`))
        .finally(() => this.arm(schedule.nextWakeDelay(this.ctx(this.clock.now()))));
    }, ms);
  }

  /** Runs every task that is due, one at a time. A tick while one is still
   * going does nothing. Resolves with the names it ran. */
  async tick(): Promise<string[]> {
    if (this.ticking) return [];
    this.ticking = true;
    const ran: string[] = [];
    try {
      for (const task of schedule.dueTasks(this.ctx(this.clock.now()))) {
        // stop() during a task (shutdown) means no further task starts.
        if (this.halted) break;
        // A task that isn't registered (history.embed without a history) is
        // not run, not recorded, and not reported as run.
        if (!this.opts.tasks[task]) continue;
        await this.runTask(task);
        ran.push(task);
      }
    } finally {
      this.ticking = false;
    }
    return ran;
  }

  private async runTask(task: string): Promise<void> {
    const started = this.clock.now();
    // Marked before running, so a failing task waits for its next turn.
    this.state.lastRun[task] = started;
    if (task === "digest") this.state.lastDigestDay = schedule.localDayKey(started);
    this.save();
    const fn = this.opts.tasks[task];
    if (!fn) return;
    this.running = task;
    let result: TaskResult;
    try {
      result = (await fn(this.taskContext())) || {};
    } catch (err) {
      result = { outcome: "failed", error: (err as Error)?.message || String(err) };
    } finally {
      this.running = null;
    }
    const ended = this.clock.now();
    const { outcome = "completed", needsLogin = false, error, ...metrics } = result;
    const cleanError = typeof error === "string" ? redactSecrets(error).slice(0, 500) : undefined;
    for (const [k, v] of Object.entries(metrics)) if (typeof v === "string") metrics[k] = redactSecrets(v).slice(0, 500);
    this.lastStatus[task] = { at: ended, outcome, durationMs: ended - started, needsLogin, ...(cleanError ? { error: cleanError } : {}) };
    if (outcome === "failed") this.log(`WARN - ${task} failed: ${cleanError || "no details"}`);
    try {
      this.opts.history?.recordEvent({
        jobId: `sched:${task}:${randomUUID()}`,
        featureId: `scheduler:${task}`,
        status: outcome,
        at: ended,
        durationMs: ended - started,
        outcome,
        metrics: { task, ...metrics, ...(needsLogin ? { needsLogin: true } : {}), ...(cleanError ? { error: cleanError } : {}) },
      });
    } catch (err) {
      this.log(`WARN - couldn't record the ${task} run: ${(err as Error).message}`);
    }
    this.save();
  }

  private taskContext(): TaskContext {
    return {
      now: () => this.clock.now(),
      quiet: this.quietNow(),
      requestClaudeRun: () => this.requestClaudeRun(),
      refundClaudeRun: () => this.refundClaudeRun(),
      trackBackgroundJob: (jobId) => this.trackBackgroundJob(jobId),
      seen: (watcher) => ({ ...(this.state.seen[watcher] || {}) }),
      setSeen: (watcher, seen) => {
        this.state.seen[watcher] = { ...seen };
        this.save();
      },
    };
  }

  /** One more background Claude run? Only one at a time, none in quiet
   * hours, and at most budget.claudeRunsPerDay a day. */
  requestClaudeRun(): ClaudeRunGrant {
    const busy = !!this.backgroundJobId && !!this.opts.isJobRunning?.(this.backgroundJobId);
    const r = schedule.grantClaudeRun({
      state: this.state,
      now: this.clock.now(),
      limit: schedule.budgetLimit(this.opts.config),
      busy,
      quiet: this.quietNow(),
    });
    if (r.granted) {
      this.state = r.state;
      this.save();
    }
    return { granted: r.granted, ...(r.reason ? { reason: r.reason } : {}), usage: r.usage };
  }

  refundClaudeRun(): void {
    this.state = schedule.refundClaudeRun({ state: this.state, now: this.clock.now() });
    this.save();
  }

  /** The background run just started — the next one waits until it ends. */
  trackBackgroundJob(jobId: string): void {
    this.backgroundJobId = jobId;
  }

  /** What Settings, doctor and GET /notifications show. */
  status(): {
    running: string | null;
    quiet: boolean;
    budget: BudgetUsage;
    watchers: Record<string, { enabled: boolean; intervalMinutes: number; last: TaskStatus | null }>;
    digest: TaskStatus | null;
  } {
    const now = this.clock.now();
    const watchers: Record<string, { enabled: boolean; intervalMinutes: number; last: TaskStatus | null }> = {};
    for (const [name, w] of Object.entries(schedule.watcherSettings(this.opts.config))) {
      watchers[name] = { ...w, last: this.lastStatus[`watcher.${name}`] || null };
    }
    return {
      running: this.running,
      quiet: this.quietNow(),
      budget: schedule.budgetUsage(this.state, now, schedule.budgetLimit(this.opts.config)),
      watchers,
      digest: this.lastStatus.digest || null,
    };
  }
}
