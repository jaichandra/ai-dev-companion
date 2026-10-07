// The three watchers as scheduler tasks (core/scheduler.ts): fetch what's
// new, filter it with core/watchers.js's rules, let the on-prem model veto
// what doesn't look worth a run (when the LLM proxy is detected), ask the
// scheduler for a Claude run, start the job and put an inbox item in.
// Sign-in is the Phase 3 chain (a cached browser session, then a saved
// token); with neither, the task reports "needs login". Every network and
// git call goes through `io`, so tests inject all of it.
import { execFile } from "child_process";
import * as fs from "fs";
import * as path from "path";
import type { Config } from "../config";
import { AuthSetupError, HttpStatusError } from "./atlassian";
import { DashboardPullRequest, IssueSummary, providersFor } from "./providers";
import type { Job } from "./jobs";
import type { LlmProxy } from "./llm-proxy";
import type { TaskContext, TaskFn, TaskResult } from "./scheduler";

interface WatcherEvent {
  watcher: string;
  key: string;
  stamp: string;
  urgent: boolean;
  url: string | null;
  pr?: DashboardPullRequest;
  issue?: IssueSummary;
}
interface Decision {
  worth: boolean;
  reason: string;
  by?: string;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const watchers = require("./watchers.js") as {
  TRIAGE_SCHEMA: Record<string, unknown>;
  prKey(pr: DashboardPullRequest): string;
  markSeen(seen: Record<string, string>, key: string, stamp: string): Record<string, string>;
  pruneSeen(seen: Record<string, string>, liveKeys: string[]): Record<string, string>;
  conflictEvents(prs: DashboardPullRequest[], merges: Record<string, { conflicted: boolean | null }>, seen: Record<string, string>): WatcherEvent[];
  conflictRule(event: WatcherEvent, facts: { featureEnabled: boolean; repoConfigured: boolean; activeJob: boolean }): Decision;
  assignedBugsJql(targets: { projects: string[]; issueTypes: string[] }): string;
  bugEvents(issues: IssueSummary[], seen: Record<string, string>): WatcherEvent[];
  bugRule(event: WatcherEvent, facts: { featureEnabled: boolean; hasCachedAnalysis: boolean; activeJob: boolean }): Decision;
  reviewEvents(prs: DashboardPullRequest[], me: string | null, seen: Record<string, string>): WatcherEvent[];
  triagePrompt(event: WatcherEvent): { system: string; messages: { role: "user"; content: string }[] };
  parseTriage(value: unknown): { worth: boolean; reason: string } | null;
  decide(rule: Decision, triage: { worth: boolean; reason: string } | null): Decision;
  notificationFor(event: WatcherEvent, outcome: Record<string, unknown>): Record<string, unknown>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const routePolicy = require("./route-policy.js") as {
  chooseTier(work: { task: string; bytes?: number; needsTools?: boolean }, env: { onpremAvailable: boolean }): { tier: "onprem" | "claude"; reason: string };
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const targets = require("./targets.js") as { analyzeTargetsFrom(config: Config): { projects: string[]; issueTypes: string[] } };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const endpoints = require("./bitbucket-endpoints.js") as { isSafeBranchName(name: unknown): boolean };

// eslint-disable-next-line @typescript-eslint/no-var-requires
const linkGuard = require("./link-guard.js") as { linkOnHosts(url: unknown, baseUrls: string[]): string | null };

const MAX_PRS = 20;
const MAX_EVENTS = 10;
const FETCH_TIMEOUT_MS = 60 * 1000;

/** Everything a watcher reads or runs outside this process. */
export interface WatcherIo {
  listDashboardPullRequests(role: "AUTHOR" | "REVIEWER"): Promise<DashboardPullRequest[]>;
  getMergeStatus(project: string, repo: string, prId: number): Promise<{ conflicted: boolean | null }>;
  searchIssues(jql: string): Promise<IssueSummary[]>;
  whoAmI(): Promise<string | null>;
  hasCachedAnalysis(issueKey: string): boolean;
  /** The configured clone of PROJECT/repo, or null. Never infers or writes config. */
  repoPath(project: string, repo: string): string | null;
  gitFetch(repoPath: string, branch: string): Promise<void>;
}

export interface WatcherDeps {
  config: Config;
  io: WatcherIo;
  llm: Pick<LlmProxy, "available" | "chat">;
  notifications: { add(input: Record<string, unknown>): unknown };
  enabledFeatureIds(): string[];
  /** A job of `featureId` for `scopeKey` that isn't finished yet. */
  activeJobFor(scopeKey: string, featureId: string): boolean;
  /** server.ts's startFeatureJob with `via: "watcher"` and ctx.background. */
  startJob(featureId: string, payload: unknown, watcher: string): Promise<Job>;
  log?(message: string): void;
}

/** A PR from a fork lives in another repository: this clone's `origin` has no such branch. */
function isFork(pr: DashboardPullRequest): boolean {
  const from = pr.fromRepo;
  if (!from || !from.projectKey || !from.slug) return false;
  return from.projectKey.toLowerCase() !== pr.project.toLowerCase() || from.slug.toLowerCase() !== pr.repo.toLowerCase();
}

function isLoginProblem(err: unknown): boolean {
  return err instanceof AuthSetupError || (err instanceof HttpStatusError && (err.status === 401 || err.status === 403));
}

function loginFailure(err: unknown): TaskResult {
  return { outcome: "failed", needsLogin: isLoginProblem(err), error: (err as Error)?.message || String(err) };
}

/** The real io: Bitbucket and Jira through the Phase 3 sign-in chain, git with no prompt. */
export function defaultWatcherIo(config: Config): WatcherIo {
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const analyzePlan = require("../features/analyze-issue/plan.js") as { readAnalysisCache(key: string): unknown };
  const { git, issues } = providersFor(config);
  return {
    listDashboardPullRequests: (role) => git.listDashboardPullRequests({}, role),
    getMergeStatus: (project, repo, prId) => git.getMergeStatus({}, project, repo, prId),
    searchIssues: (jql) => issues.searchIssues({}, jql, 20),
    whoAmI: () => git.whoAmI({}).catch(() => null),
    hasCachedAnalysis: (key) => {
      try {
        return analyzePlan.readAnalysisCache(key) !== null;
      } catch {
        return false;
      }
    },
    repoPath: (project, repo) => {
      const want = `${project}/${repo}`.toLowerCase();
      const key = Object.keys(config.repos || {}).find((k) => k.toLowerCase() === want);
      const dir = key ? config.repos[key] : null;
      return dir && fs.existsSync(path.join(dir, ".git")) ? dir : null;
    },
    gitFetch: (repoPath, branch) =>
      new Promise((resolve, reject) => {
        if (!endpoints.isSafeBranchName(branch)) {
          reject(new Error("not a safe branch name"));
          return;
        }
        execFile(
          "git",
          ["-C", repoPath, "fetch", "--no-tags", "origin", `+refs/heads/${branch}:refs/remotes/origin/${branch}`],
          { timeout: FETCH_TIMEOUT_MS, env: { ...process.env, GIT_TERMINAL_PROMPT: "0" } },
          (err) => (err ? reject(new Error(`git fetch failed: ${err.message.split("\n")[0]}`)) : resolve()),
        );
      }),
  };
}

export function createWatcherRunner(deps: WatcherDeps): Record<"conflicts" | "assignedBugs" | "reviewRequests", TaskFn> {
  const log = deps.log || ((m: string) => console.log(`[watchers] ${m}`));

  /** An inbox item opens a link on click: only https on the configured Bitbucket or Jira host. */
  function withSafeUrl(event: WatcherEvent): WatcherEvent {
    let url: string | null = null;
    try {
      url = linkGuard.linkOnHosts(event.url, [providersFor(deps.config).git.baseUrl(), providersFor(deps.config).issues.baseUrl()]);
    } catch {
      url = null;
    }
    return { ...event, url };
  }

  /** The on-prem veto, when the proxy is there; null means "rules only". */
  async function triage(event: WatcherEvent, stats: { onprem: number; rules: number; failed: number }, onprem: boolean) {
    const prompt = watchers.triagePrompt(event);
    const bytes = Buffer.byteLength(prompt.system + prompt.messages[0].content);
    if (routePolicy.chooseTier({ task: "triage", bytes }, { onpremAvailable: onprem }).tier !== "onprem") {
      stats.rules++;
      return null;
    }
    try {
      const reply = await deps.llm.chat({ ...prompt, responseSchema: watchers.TRIAGE_SCHEMA, maxTokens: 1024 });
      const parsed = watchers.parseTriage(reply.value);
      if (parsed) {
        stats.onprem++;
        return parsed;
      }
    } catch (err) {
      log(`WARN - on-prem triage failed, using the rules alone: ${(err as Error).message}`);
    }
    stats.failed++;
    return null;
  }

  /** Decide, ask for a run, start it, and file the inbox item. Returns
   * whether the event is settled (false: "busy", try again next tick). */
  async function act(
    ctx: TaskContext,
    event: WatcherEvent,
    decision: Decision,
    start: { featureId: string; payload: unknown },
    counts: { started: number; inboxed: number; deferred: number },
  ): Promise<boolean> {
    let outcome: Record<string, unknown> = { kind: "skipped", reason: decision.reason };
    if (decision.worth) {
      const grant = ctx.requestClaudeRun();
      if (!grant.granted && grant.reason === "busy") {
        counts.deferred++;
        return false;
      }
      if (!grant.granted) {
        outcome = { kind: "skipped", reason: grant.reason };
      } else {
        try {
          const job = await deps.startJob(start.featureId, start.payload, event.watcher);
          ctx.trackBackgroundJob(job.id);
          counts.started++;
          outcome = { kind: "prewarmed", jobId: job.id, featureId: start.featureId };
        } catch (err) {
          // The run never began: it must not cost the day's budget.
          ctx.refundClaudeRun();
          outcome = { kind: "skipped", reason: `it couldn't start (${(err as Error).message.slice(0, 160)})` };
        }
      }
    }
    try {
      deps.notifications.add(watchers.notificationFor(event, outcome));
      counts.inboxed++;
    } catch (err) {
      log(`WARN - couldn't add an inbox item: ${(err as Error).message}`);
    }
    return true;
  }

  const conflicts: TaskFn = async (ctx) => {
    let prs: DashboardPullRequest[];
    let allPrs: DashboardPullRequest[];
    try {
      allPrs = await deps.io.listDashboardPullRequests("AUTHOR");
      prs = allPrs.slice(0, MAX_PRS);
    } catch (err) {
      return loginFailure(err);
    }
    const merges: Record<string, { conflicted: boolean | null }> = {};
    let mergeErrors = 0;
    for (const pr of prs) {
      try {
        merges[watchers.prKey(pr)] = await deps.io.getMergeStatus(pr.project, pr.repo, pr.id);
      } catch {
        mergeErrors++;
      }
    }
    let seen = ctx.seen("conflicts");
    const events = watchers.conflictEvents(prs, merges, seen).slice(0, MAX_EVENTS).map(withSafeUrl);
    const onprem = events.length > 0 && !ctx.quiet ? await deps.llm.available().catch(() => false) : false;
    const stats = { onprem: 0, rules: 0, failed: 0 };
    const counts = { started: 0, inboxed: 0, deferred: 0 };
    const enabled = deps.enabledFeatureIds().includes("resolve-conflict");
    for (const event of events) {
      const pr = event.pr as DashboardPullRequest;
      const rule = watchers.conflictRule(event, {
        featureEnabled: enabled,
        repoConfigured: deps.io.repoPath(pr.project, pr.repo) !== null,
        activeJob: deps.activeJobFor(event.key, "resolve-conflict"),
      });
      const forkRule = rule.worth && isFork(pr) ? { worth: false, reason: "it comes from a fork, which this clone can't fetch" } : rule;
      const decision = watchers.decide(forkRule, forkRule.worth ? await triage(event, stats, onprem) : null);
      const payload = { project: pr.project, repo: pr.repo, prId: pr.id, sourceBranch: pr.fromBranch, destBranch: pr.toBranch };
      if (await act(ctx, event, decision, { featureId: "resolve-conflict", payload }, counts)) {
        seen = watchers.markSeen(seen, event.key, event.stamp);
        // Saved per settled event: a crash mid-loop must not start the same job again.
        ctx.setSeen("conflicts", seen);
      }
    }
    // Pruned against every PR the dashboard returned, not just the 20 looked at.
    ctx.setSeen("conflicts", watchers.pruneSeen(seen, allPrs.map((p) => watchers.prKey(p))));
    return { found: prs.length, events: events.length, ...counts, mergeErrors, triage: stats, tier: stats.onprem > 0 ? "onprem" : "rules" };
  };

  const assignedBugs: TaskFn = async (ctx) => {
    let issues: IssueSummary[];
    try {
      issues = await deps.io.searchIssues(watchers.assignedBugsJql(targets.analyzeTargetsFrom(deps.config)));
    } catch (err) {
      return loginFailure(err);
    }
    let seen = ctx.seen("assignedBugs");
    const events = watchers.bugEvents(issues, seen).slice(0, MAX_EVENTS).map(withSafeUrl);
    const onprem = events.length > 0 && !ctx.quiet ? await deps.llm.available().catch(() => false) : false;
    const stats = { onprem: 0, rules: 0, failed: 0 };
    const counts = { started: 0, inboxed: 0, deferred: 0 };
    const enabled = deps.enabledFeatureIds().includes("analyze-issue");
    for (const event of events) {
      const rule = watchers.bugRule(event, {
        featureEnabled: enabled,
        hasCachedAnalysis: deps.io.hasCachedAnalysis(event.key),
        activeJob: deps.activeJobFor(`jira:${event.key}`, "analyze-issue"),
      });
      const decision = watchers.decide(rule, rule.worth ? await triage(event, stats, onprem) : null);
      if (await act(ctx, event, decision, { featureId: "analyze-issue", payload: { issueKey: event.key } }, counts)) {
        seen = watchers.markSeen(seen, event.key, event.stamp);
        ctx.setSeen("assignedBugs", seen);
      }
    }
    // (markSeen keeps at most 500 entries; a ticket that drops out of the "updated in the last day" list is not forgotten, or it would come back as new.)
    ctx.setSeen("assignedBugs", seen);
    return { found: issues.length, events: events.length, ...counts, triage: stats, tier: stats.onprem > 0 ? "onprem" : "rules" };
  };

  const reviewRequests: TaskFn = async (ctx) => {
    let prs: DashboardPullRequest[];
    let allPrs: DashboardPullRequest[];
    try {
      allPrs = await deps.io.listDashboardPullRequests("REVIEWER");
      prs = allPrs.slice(0, MAX_PRS);
    } catch (err) {
      return loginFailure(err);
    }
    const me = await deps.io.whoAmI().catch(() => null);
    let seen = ctx.seen("reviewRequests");
    const events = watchers.reviewEvents(prs, me, seen).slice(0, MAX_EVENTS).map(withSafeUrl);
    let fetched = 0;
    for (const event of events) {
      const pr = event.pr as DashboardPullRequest;
      const repoPath = deps.io.repoPath(pr.project, pr.repo);
      let outcome: Record<string, unknown>;
      if (!repoPath) outcome = { kind: "fetch-failed", reason: "no local clone is set up" };
      else if (isFork(pr)) outcome = { kind: "fetch-failed", reason: "it comes from a fork, which this clone can't fetch" };
      else if (!pr.fromBranch || !endpoints.isSafeBranchName(pr.fromBranch)) outcome = { kind: "fetch-failed", reason: "its branch name can't be fetched safely" };
      else {
        try {
          await deps.io.gitFetch(repoPath, pr.fromBranch);
          fetched++;
          outcome = { kind: "fetched" };
        } catch (err) {
          outcome = { kind: "fetch-failed", reason: (err as Error).message.slice(0, 160) };
        }
      }
      try {
        deps.notifications.add(watchers.notificationFor(event, outcome));
      } catch (err) {
        log(`WARN - couldn't add an inbox item: ${(err as Error).message}`);
      }
      seen = watchers.markSeen(seen, event.key, event.stamp);
      ctx.setSeen("reviewRequests", seen);
    }
    ctx.setSeen("reviewRequests", watchers.pruneSeen(seen, allPrs.map((p) => watchers.prKey(p))));
    return { found: prs.length, events: events.length, fetched };
  };

  return { conflicts, assignedBugs, reviewRequests };
}
