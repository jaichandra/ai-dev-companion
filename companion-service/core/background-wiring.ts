// How the background work is put together, out of server.ts so it can be
// tested: which context a feature start gets, the watchers' hand-off to
// startFeatureJob, the scheduler with its tasks (including Phase 8's
// history.embed, only when the local history is on), and the inbox routes.
// Everything with a side effect comes in through `deps`.
import type express from "express";
import type { Config } from "../config";
import type { Feature, FeatureContext, FeatureDeps, Job } from "./jobs";
import type { HistoryStore } from "./history";
import type { LlmProxy } from "./llm-proxy";
import { Scheduler } from "./scheduler";
import type { Clock, TaskFn } from "./scheduler";
import { createWatcherRunner } from "./watcher-runner";
import type { WatcherIo } from "./watcher-runner";
import { createDigestTask } from "../features/digest";

export type StartVia = "extension" | "mcp" | "watcher";

/** Job states in which a job for the same PR or ticket still counts as open:
 * a watcher must not start a second one. */
export const ACTIVE_JOB_STATUSES = ["running", "awaiting-approval", "approving", "rejecting", "pending-start"];

/** The context a feature start really gets. `background` is honoured only for
 * the watcher's own call, whatever a caller put in the context, so a request
 * (extension or MCP) can never select the background policy. */
export function featureContextFor(via: StartVia, ctx: FeatureContext): FeatureContext {
  return { ...ctx, background: via === "watcher" && ctx.background === true };
}

/** Builds the enabled features, handing every factory the same deps (the
 * history and, with it on, the shared similar-item search). Out of server.ts
 * so a test can prove analyze-issue really receives `findSimilar`. */
export function buildFeatures(
  ids: string[],
  factories: Record<string, (config: Config, deps: FeatureDeps) => Feature>,
  config: Config,
  deps: FeatureDeps,
): Feature[] {
  return ids.filter((id) => factories[id]).map((id) => factories[id](config, deps));
}

export interface InboxStore {
  add(input: Record<string, unknown>): unknown;
  list(opts: { includeSeen?: boolean; limit?: number }): unknown[];
  unseenCount(): number;
  markSeen(ids: string[] | "all"): number;
  get(id: string): { openedAt?: number | null } | null;
  open(id: string): { jobId?: string } | null;
  announce(opts: { quiet: boolean; minIntervalMs: number }): unknown;
  absorbPending(): string[];
}

type UsedEvent = (job: Job | undefined, at: number) => Parameters<HistoryStore["recordEvent"]>[0] | null;

// eslint-disable-next-line @typescript-eslint/no-var-requires
const historyRecord = require("./history-record.js") as { usedEvent: UsedEvent };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const scheduleRules = require("./schedule.js") as { notifyMinIntervalMs(config: Config): number };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const embedTask = require("./embed-task.js") as {
  createEmbedTask(deps: { history: HistoryStore; llm: Pick<LlmProxy, "detect" | "embed" | "settings">; getConfig: () => Config }): TaskFn;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const backgroundRoutes = require("./background-routes.js") as {
  createBackgroundRoutes(deps: {
    inbox: InboxStore;
    scheduler: Scheduler;
    history?: HistoryStore;
    usedEvent: UsedEvent;
    getJob(id: string): Job | undefined;
    minIntervalMs(): number;
    log?(m: string): void;
  }): Record<"list" | "seen" | "announce" | "open", express.RequestHandler>;
};

export interface BackgroundWiringDeps {
  /** The one live config object (Settings saves mutate it in place). */
  config: Config;
  features: Feature[];
  jobStore: { list(): Job[]; get(id: string): Job | undefined };
  /** server.ts's startFeatureJob. */
  startFeatureJob(feature: Feature, payload: unknown, ctx: FeatureContext, via: StartVia, watcher?: string): Promise<Job>;
  inbox: InboxStore;
  history?: HistoryStore;
  llm: Pick<LlmProxy, "available" | "chat" | "detect" | "embed" | "settings">;
  io: WatcherIo;
  clock?: Clock;
  statePath: string | null;
  /** The daily job-expiry and history-pruning pass. */
  maintenance: TaskFn;
  /** True once the service is shutting down: no new background jobs start. */
  isShuttingDown?(): boolean;
  log?(message: string): void;
}

export function createBackgroundWiring(deps: BackgroundWiringDeps) {
  const activeJobFor = (key: string, featureId: string): boolean =>
    deps.jobStore.list().some((j) => j.scopeKey === key && j.featureId === featureId && ACTIVE_JOB_STATUSES.includes(j.status));

  // The watchers start jobs exactly as a click would (same readiness and repo
  // checks), but marked `startedVia: "watcher"` and with ctx.background, so a
  // feature that runs Claude with no click behind it gets the stricter
  // readOnlyBackground policy. Nothing here approves or pushes: Resolve
  // Conflict does run in its sandboxed worktree without a click, but stops at
  // "awaiting-approval" — only a person's Approve pushes.
  const watcherRunner = createWatcherRunner({
    config: deps.config,
    io: deps.io,
    llm: deps.llm,
    notifications: deps.inbox,
    enabledFeatureIds: () => deps.features.map((f) => f.id),
    activeJobFor,
    startJob: async (featureId, payload, watcher) => {
      if (deps.isShuttingDown?.()) throw new Error("The service is shutting down.");
      const feature = deps.features.find((f) => f.id === featureId);
      if (!feature) throw new Error(`${featureId} isn't enabled.`);
      return deps.startFeatureJob(feature, payload, { auth: {}, body: payload, background: true }, "watcher", watcher);
    },
    ...(deps.log ? { log: deps.log } : {}),
  });

  const scheduler = new Scheduler({
    config: deps.config,
    statePath: deps.statePath,
    history: deps.history,
    ...(deps.clock ? { clock: deps.clock } : {}),
    isJobRunning: (id) => deps.jobStore.get(id)?.status === "running",
    ...(deps.log ? { log: deps.log } : {}),
    tasks: {
      maintenance: deps.maintenance,
      "watcher.conflicts": watcherRunner.conflicts,
      "watcher.assignedBugs": watcherRunner.assignedBugs,
      "watcher.reviewRequests": watcherRunner.reviewRequests,
      digest: createDigestTask({ config: deps.config, notifications: deps.inbox }),
      // Embeds new history items on-prem for similar-item search; with no
      // history there is nothing to embed, so the task isn't registered.
      ...(deps.history ? { "history.embed": embedTask.createEmbedTask({ history: deps.history, llm: deps.llm, getConfig: () => deps.config }) } : {}),
    },
  });

  const routes = backgroundRoutes.createBackgroundRoutes({
    inbox: deps.inbox,
    scheduler,
    history: deps.history,
    usedEvent: historyRecord.usedEvent,
    getJob: (id) => deps.jobStore.get(id),
    minIntervalMs: () => scheduleRules.notifyMinIntervalMs(deps.config),
    log: (m) => console.log(`[notifications] WARN - ${m}`),
  });

  return { watcherRunner, scheduler, routes, activeJobFor };
}
