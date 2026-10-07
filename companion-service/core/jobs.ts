import { randomUUID } from "crypto";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const jobFiles = require("./job-files.js") as {
  jobsDir(stateDir: string): string;
  saveJob(dir: string, job: Job): void;
  removeJob(dir: string, id: string): void;
  persistAction(job: Job, featureIds: string[]): "skip" | "save" | "remove";
  loadJobs(
    dir: string,
    opts: {
      now?: number;
      worktreeRoot: string;
      maxAgeMs?: number;
      sessionCwdOk?: (cwd: string) => boolean;
      log?: (msg: string) => void;
    },
  ): { jobs: Job[]; reconciled: string[]; pruned: string[]; prunedJobs: Job[]; dropped: string[] };
  isStale(job: Job, now: number, maxAgeMs?: number): boolean;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const scopeKey = require("./scope-key.js") as {
  lookupJob(jobs: Job[], scopeKey: string, featureId: string): Job | null;
};

export type JobStatus =
  | "running"
  | "awaiting-approval"
  | "approving"
  | "approved"
  | "rejecting"
  | "rejected"
  | "failed"
  | "pending-start";

export interface FileDiff {
  path: string;
  /** Unified-diff-ish text; simple line-based, not a full patch format. */
  diff: string;
  /** Optional human-readable caveat about what `diff` actually shows —
   * e.g. resolve-conflict's renderReview sets this when a conflicted file
   * was resolved wholesale to one side, so the diff alone (against
   * preMergeSha or destSha, whichever isn't empty) wouldn't otherwise
   * make that clear. Rendered by the extension (the extension feature file (chrome-extension/features/<id>.js)'s
   * resolve-conflict renderPanel). */
  note?: string;
}

/** A headless `claude` run a feature kept alive by cwd/session id rather
 * than tearing down (see analyze-issue's runAnalyze) — enough for a later
 * phase to `--resume` the same transcript in the same directory. */
export interface ClaudeSession {
  id: string;
  cwd: string;
  permissionMode: "plan" | "auto" | "default";
}

export interface JobProgress {
  /** Machine-readable phase id, e.g. "merge" — the extension's feature
   * registry declares the ordered list of phases per feature so it can
   * render a checklist (done/current/pending) purely from this id. */
  stepId: string;
  /** Human-readable text for the *current* step specifically — can carry
   * live detail (e.g. a file count) that a static per-feature label can't. */
  label: string;
}

export interface Job {
  id: string;
  featureId: string;
  status: JobStatus;
  /** Free-form payload the feature stashes for its own approve()/reject(). */
  data: Record<string, unknown>;
  /** Where the job is *while still running* — cleared meaning is implied by
   * status once the job reaches a terminal state. */
  progress?: JobProgress;
  /** Populated once the feature has something for the user to review. */
  result?: {
    summary: string;
    files: FileDiff[];
    /** Feature-specific structured result (e.g. review-in-editor's panel data). */
    review?: Record<string, unknown>;
  };
  error?: string;
  createdAt: number;
  updatedAt: number;
  /** Which browser page this job belongs to (a Bitbucket PR, a Jira issue,
   * the pre-deployment-stats Jenkins job) — see core/scope-key.js's
   * scopeKeyFor. Set by server.ts's startFeatureJob once the job exists;
   * absent for a job whose feature doesn't tag jobs this way. */
  scopeKey?: string;
  /** Who kicked this job off: a browser Start click ("extension") or an
   * MCP tool call ("mcp") — including a "pending-start" job's eventual
   * real start once POST /jobs/:jobId/start runs it. Absent means
   * "extension" (every job before Task 5 was one). */
  startedVia?: "extension" | "mcp" | "watcher";
  /** The watcher that pre-warmed this job (startedVia "watcher"), e.g.
   * "conflicts" — recorded in its history events for the "pre-warmed runs
   * used" metric. */
  watcher?: string;
}

/**
 * In-memory job store, with opt-in write-through persistence to disk (off
 * by default, and in every test). Persistence is a separate concern the
 * store just calls into — see core/job-files.js for the on-disk format,
 * reconciliation of a job caught mid-flight by a restart, and pruning of
 * stale ones. `enablePersistence` turns it on and loads what's there;
 * until then (or for a feature that never opts in — see Decision 1 in
 * docs/superpowers/plans/2026-09-27-v0.8.1-persist-jobs.md) this behaves
 * exactly as the old in-memory-only store did: a restart mid-job just
 * means the user re-clicks the button.
 */
class JobStore {
  private jobs = new Map<string, Job>();
  /** Set by enablePersistence; undefined means "persistence is off" —
   * checked by persist() so create/update/patchData stay no-ops until then. */
  private persistence?: { dir: string; featureIds: string[] };
  private transitionListeners: ((job: Job, from: JobStatus) => void)[] = [];

  /** Calls `listener` after a job's status changes (from update()). A
   * listener that throws is logged and ignored — history must never break a job. */
  onTransition(listener: (job: Job, from: JobStatus) => void): void {
    this.transitionListeners.push(listener);
  }

  private notifyTransition(job: Job, from: JobStatus): void {
    for (const listener of this.transitionListeners) {
      try {
        listener(job, from);
      } catch (err) {
        console.log(`[jobs] WARN - transition listener failed: ${(err as Error).message}`);
      }
    }
  }

  create(featureId: string, data: Record<string, unknown> = {}): Job {
    const now = Date.now();
    const job: Job = {
      id: randomUUID(),
      featureId,
      status: "running",
      data,
      createdAt: now,
      updatedAt: now,
    };
    this.jobs.set(job.id, job);
    this.persist(job);
    return job;
  }

  /** Creates a job that's waiting for a browser Start click before it
   * actually runs (see server.ts's POST /jobs/:jobId/start) — how an MCP
   * tool with write access hands off to the extension instead of ever
   * driving Claude with Bash access on its own. `data` is `{ payload }`,
   * the same payload a real feature.start() would have received, so
   * starting it later is just `feature.start(job.data.payload, ...)`. */
  createPending(featureId: string, payload: unknown, scopeKeyValue: string | undefined): Job {
    const now = Date.now();
    const job: Job = {
      id: randomUUID(),
      featureId,
      status: "pending-start",
      data: { payload },
      scopeKey: scopeKeyValue,
      startedVia: "mcp",
      createdAt: now,
      updatedAt: now,
    };
    this.jobs.set(job.id, job);
    this.persist(job);
    return job;
  }

  get(id: string): Job | undefined {
    return this.jobs.get(id);
  }

  /** Every job, newest first — GET /jobs/lookup's fallback shape (and any
   * future caller) doesn't have to sort in-memory jobs itself. */
  list(): Job[] {
    return [...this.jobs.values()].sort((a, b) => b.createdAt - a.createdAt);
  }

  /** The newest still-relevant job an MCP tool started for `scopeKey` on
   * `featureId` — see core/scope-key.js's lookupJob for exactly what
   * counts. Used by GET /jobs/lookup so a page can find its own
   * "pending-start" job (or a since-started one still awaiting review)
   * without the extension having to remember the job id itself. */
  lookup(scopeKeyValue: string, featureId: string): Job | null {
    return scopeKey.lookupJob([...this.jobs.values()], scopeKeyValue, featureId);
  }

  /** Puts back a job a feature kept on disk across a restart (see
   * review-in-editor's session records); an existing job wins. Also runs
   * the job through persist() — a no-op for review-in-editor today, since
   * it isn't in the persisted featureIds, but this keeps restore() from
   * silently diverging from create()/update() if that ever changes. */
  restore(job: Job): Job {
    const existing = this.jobs.get(job.id);
    if (existing) return existing;
    this.jobs.set(job.id, job);
    this.persist(job);
    return job;
  }

  update(id: string, patch: Partial<Job>): Job {
    const job = this.jobs.get(id);
    if (!job) {
      throw new Error(`Unknown job id: ${id}`);
    }
    const from = job.status;
    Object.assign(job, patch, { updatedAt: Date.now() });
    this.persist(job);
    if (job.status !== from) this.notifyTransition(job, from);
    return job;
  }

  /** Sets the progress label only while the job is still running — a
   * throttled label can arrive just after a cancel or a failure, and must not
   * overwrite that state. */
  setProgressIfRunning(id: string, stepId: string, label: string): void {
    const job = this.jobs.get(id);
    if (!job || job.status !== "running") return;
    this.update(id, { progress: { stepId, label } });
  }

  /** Shallow-merges `partial` into job.data (unlike update(), which would
   * replace the whole `data` object if given one) — for a caller that
   * wants to add or overwrite a single field, e.g. analyze-issue stamping
   * `claudeSession` onto a job whose `data` was already built elsewhere. */
  patchData(id: string, partial: Record<string, unknown>): Job {
    const job = this.jobs.get(id);
    if (!job) {
      throw new Error(`Unknown job id: ${id}`);
    }
    job.data = { ...job.data, ...partial };
    job.updatedAt = Date.now();
    this.persist(job);
    return job;
  }

  /** How many jobs are mid-flight — work a service restart would cut off. */
  activeCount(): number {
    return [...this.jobs.values()].filter((j) => ["running", "approving", "rejecting"].includes(j.status)).length;
  }

  /**
   * Turns on write-through persistence and loads whatever's already on
   * disk under `dir` (only for `featureIds` — see core/job-files.js's
   * persistAction). Loaded jobs go through restore(), so a job already in
   * the in-memory Map (there shouldn't be one this early, but a future
   * caller might load twice) wins over the on-disk copy. Returns the same
   * `{ loaded, reconciled, pruned, prunedJobs, dropped }` loadJobs reports, with
   * `loaded` being `jobs.length`, for server.ts's startup log line.
   */
  enablePersistence(opts: {
    dir: string;
    worktreeRoot: string;
    featureIds: string[];
    sessionCwdOk?: (cwd: string) => boolean;
  }): { loaded: number; reconciled: string[]; pruned: string[]; prunedJobs: Job[]; dropped: string[] } {
    const { dir, worktreeRoot, featureIds, sessionCwdOk } = opts;
    const { jobs, reconciled, pruned, prunedJobs, dropped } = jobFiles.loadJobs(dir, {
      worktreeRoot,
      sessionCwdOk,
      log: (m: string) => console.log(`[jobs] WARN - ${m}`),
    });
    for (const job of jobs) {
      this.restore(job);
    }
    // restore() already ran persist() for each loaded job above, but that
    // happened before persistence was turned on below — nothing was
    // written yet, and a reconciled job's on-disk copy is already correct
    // (loadJobs saved it itself). Turning persistence on now, after the
    // load, means write-through starts from here on.
    this.persistence = { dir, featureIds };
    return { loaded: jobs.length, reconciled, pruned, prunedJobs, dropped };
  }

  /**
   * Stops write-through for the rest of this process's life (the in-memory
   * store keeps working). server.ts's shutdown handler calls this before
   * killing the detached claude children: each dying run's catch would
   * otherwise save its job as "failed" with whatever error the killed
   * child produced ("aborted", an exit code), and the restarted service
   * would show that instead of reconciling the on-disk "running" job to
   * the restart message that tells the user what to do next.
   */
  freezePersistence(): void {
    this.persistence = undefined;
  }

  /**
   * Drops persisted jobs nobody acted on for `maxAgeMs` — from memory and
   * from disk — and returns them so the caller can remove their worktrees.
   * loadJobs prunes at startup; a service that runs for weeks needs this too.
   * Only persisted features' awaiting-approval/failed jobs are ever swept.
   */
  sweepStale(maxAgeMs: number, now: number = Date.now()): Job[] {
    if (!this.persistence) return [];
    const { dir, featureIds } = this.persistence;
    const swept: Job[] = [];
    for (const job of [...this.jobs.values()]) {
      if (!featureIds.includes(job.featureId) || !jobFiles.isStale(job, now, maxAgeMs)) continue;
      this.jobs.delete(job.id);
      try {
        jobFiles.removeJob(dir, job.id);
      } catch (err) {
        console.log(`[jobs] WARN - couldn't remove the file of stale job ${job.id}: ${(err as Error).message}`);
      }
      swept.push(job);
    }
    return swept;
  }

  /**
   * Write-through for a single job, called at the end of create/update/
   * patchData/restore. A no-op while persistence is off (the default).
   * Never throws — a save failure only costs this job's restart
   * resilience (Decision 8), not the job itself, so it's caught and
   * logged as a WARN instead of propagating into the caller's request
   * handling.
   */
  private persist(job: Job): void {
    if (!this.persistence) return;
    const { dir, featureIds } = this.persistence;
    const action = jobFiles.persistAction(job, featureIds);
    if (action === "skip") return;
    try {
      if (action === "remove") {
        jobFiles.removeJob(dir, job.id);
      } else {
        jobFiles.saveJob(dir, job);
      }
    } catch (err) {
      console.log(`[jobs] WARN - couldn't save job ${job.id}: ${(err as Error).message}`);
    }
  }
}

export const jobStore = new JobStore();

/** Thrown by a feature's start() to refuse with a structured 409 (`code`
 * plus `details`) the extension can act on, instead of a failed job. */
export class StartRefusedError extends Error {
  constructor(
    message: string,
    readonly code: string,
    readonly details: Record<string, unknown> = {},
  ) {
    super(message);
  }
}

/** Thrown by an optional feature method (e.g. refreshDiff) when the job
 * isn't in a status that operation supports right now — maps to 409 in
 * server.ts's sendError, the same status StartRefusedError gets for a
 * refused start(). */
export class JobStatusError extends Error {}

/** Thrown by an optional feature method (e.g. refreshDiff) when the job's
 * worktree (or other on-disk resource the job depends on) has already
 * been removed — maps to 410 (Gone) in server.ts's sendError. */
export class WorktreeGoneError extends Error {}

/** Per-request context passed alongside a feature's start/approve/reject —
 * additive to the original two-argument shape, so a feature that doesn't
 * need it (e.g. resolve-conflict, which only ever talks to git) can ignore
 * the second parameter entirely. */
export interface FeatureContext {
  /** The page's browser session, relayed from the extension (see
   * background.js's relayHeadersFor / core/atlassian.ts's authedJson) —
   * how a feature acts as the logged-in user against e.g. Jira without any
   * credential being configured. */
  auth: import("./atlassian").AuthContext;
  /** The request body for this call — approve()/reject() don't otherwise
   * receive one, but a feature like create-jira-subtasks needs the panel's
   * in-review edits (e.g. corrected assignees) on approve. */
  body?: unknown;
  /** Set only by the service's own watchers (core/watcher-runner.ts), never
   * from a request: the run has no click behind it, so a feature that runs
   * Claude uses the stricter `readOnlyBackground` policy. */
  background?: boolean;
}

/** Contract every plugged-in feature implements. The core never inspects
 * a feature's internals beyond this shape — see companion-service/README
 * (or the top-level plan) for why this keeps adding features additive. */
export interface Feature {
  id: string;
  label: string;
  /** Kick off a job from an extension-supplied payload. Should create the
   * Job via jobStore.create(feature.id, ...) and drive it to
   * "awaiting-approval" or "failed" (async work may continue after return —
   * callers poll GET /status/:jobId). */
  start(payload: unknown, ctx: FeatureContext): Promise<Job>;
  /** Apply the job's proposed result (e.g. commit + push, or create Jira
   * issues). Must transition the job to "approved" or "failed". */
  approve(job: Job, ctx: FeatureContext): Promise<void>;
  /** Discard the job's proposed result and clean up any resources it
   * created (worktrees, temp branches, ...). Must transition the job to
   * "rejected" or "failed". */
  reject(job: Job, ctx: FeatureContext): Promise<void>;
  /** Optional: stop a still-"running" job, undo whatever it has changed so
   * far, and resolve once that cleanup is done, with the job "rejected".
   * Only features that implement this get a Cancel button while running. */
  cancel?(job: Job, ctx: FeatureContext): Promise<void>;
  /** Optional: post a Jira comment for the job (analyze-issue). Expects
   * Markdown on `ctx.body.body`; does not transition job status. Only
   * features that implement this get a POST /features/<id>/:jobId/post-comment
   * route. */
  postComment?(job: Job, ctx: FeatureContext): Promise<{ commentId?: string }>;
  /** Optional: live status of whatever a finished job handed off outside
   * this service (review-in-editor's terminal review). Returned as
   * `session` alongside the job by GET /status/:jobId; undefined when the
   * job has nothing to track. */
  sessionStatus?(job: Job): Promise<SessionStatus | undefined>;
  /** Optional: stop that session. POST /features/<id>/:jobId/cancel calls
   * this for an "approved" job. Resolves once it has stopped. */
  stopSession?(job: Job): Promise<void>;
  /** Optional: re-check and re-render the job's diff/result in place —
   * for a job whose worktree was handed off to a resumed terminal session
   * (see core/terminal.ts) where the user may have kept editing after the
   * panel last rendered. Only features that implement this get a generic
   * POST /features/<id>/:jobId/refresh-diff route (see server.ts). Should
   * throw JobStatusError if the job isn't in a status this supports right
   * now, or WorktreeGoneError if the job's worktree no longer exists.
   * Resolves with the updated job. */
  refreshDiff?(job: Job, ctx: FeatureContext): Promise<Job>;
  /** Optional: the MCP tools this feature exposes to terminal Claude (see
   * Task 9's MCP server). Declared here, filled in by Task 9 — declaring
   * the types now keeps that task's diff focused on the tools themselves
   * rather than also introducing this shape. */
  mcpTools?(): McpToolDef[];
  /** Optional: named actions on one of this feature's jobs, served at
   * POST /features/<id>/:jobId/actions/<name> (server.ts). `ctx.body` is
   * the request body. Each resolves with what the route returns (usually
   * the updated job); throw JobStatusError (409) or WorktreeGoneError (410)
   * as refreshDiff does. Anything a body names (a folder, a session) must
   * be checked against the job's own server-held data, never trusted. */
  actions?: Record<string, (job: Job, ctx: FeatureContext) => Promise<unknown>>;
  /** Optional: the job as the extension should see it. Job data can come
   * from a persisted file, so a feature that hands the page an address (a
   * link it will open) re-checks it here on every read. Must not mutate
   * `job`. */
  present?(job: Job): Job;
  /** Optional: background work this feature needs while the service runs
   * (a poll). Started by server.ts on the installed copy only; returns how to
   * stop it. */
  startBackground?(): () => void;
}

/** One similar past ticket or PR (core/similar-search.js). Its text is
 * untrusted: titles are written by people, analyses are earlier AI output. */
export interface SimilarEntry {
  key: string;
  kind: string;
  title: string | null;
  analysis: string | null;
  /** The repo the ticket was analyzed in, when that was recorded. */
  repo?: string | null;
  updatedAt: number | null;
  score: number;
  via: string[];
}

export type FindSimilar = (query: { key?: string; text?: string; k?: number }) => Promise<{
  enabled: boolean;
  mode: "off" | "text" | "vector+text";
  items: SimilarEntry[];
}>;

/** What server.ts hands every feature factory besides the config. */
export interface FeatureDeps {
  /** The git host, issue tracker and CI server this install talks to (core/providers.ts). */
  providers: import("./providers").Providers;
  /** The local history, or undefined when it is off (a dev checkout). */
  history?: import("./history").HistoryStore;
  /** Similar past tickets from the local history (Phase 8); undefined when the history is off. */
  findSimilar?: FindSimilar;
}

/** One MCP tool a feature exposes, alongside the name the MCP server
 * registers it under. */
export interface McpToolDef {
  name: string;
  handler(args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown>;
}

/** What an MCP tool handler gets instead of FeatureContext — an MCP tool
 * call has no browser request of its own to relay auth from or read a
 * body off, and it needs to start/await jobs rather than being handed one
 * that already exists. */
export interface McpToolContext {
  /** The browser session cached from whatever the extension last relayed
   * (see core/auth-context.ts) — an MCP tool call has no request of its
   * own to relay one from. */
  auth: import("./atlassian").AuthContext;
  /** Runs a feature's start() the same way the extension's own
   * POST /features/:id/start does (server.ts's startFeatureJob, tagged
   * `startedVia: "mcp"`) — for a read-only tool that's safe to run without
   * a browser Start click. */
  startJob(featureId: string, payload: unknown): Promise<Job>;
  /** Creates a "pending-start" job instead of actually running the
   * feature — for a tool that would give Claude Bash access; see
   * JobStore.createPending. */
  createPendingJob(featureId: string, payload: unknown): Job;
  /** Polls jobStore.get(jobId) until it leaves "running"/"approving"/
   * "rejecting" or `timeoutMs` elapses. */
  awaitJob(jobId: string, timeoutMs: number): Promise<Job>;
}

export interface SessionStatus {
  /** "starting" | "running" | "finished" | "cancelled" | "ended". */
  state: string;
  exitCode?: number;
}
