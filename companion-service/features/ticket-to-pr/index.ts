// Ticket to PR. Start fix (start): the repo from the ticket's saved
// analysis, a branch named from the ticket, the base from Bitbucket's
// default branch, a persistent worktree <repo>.worktrees/<KEY>, and Claude
// Code in plan mode with the analysis as its prompt. The job then waits in
// awaiting-approval while you work in that terminal: commit, push and open
// the pull request there. Once a PR from the branch is open, review-mover.ts
// moves the ticket to its review status by itself (or on the panel's
// "Move ticket to In Review") and ends the job as approved. The Create PR
// path (approve, with a title and description) is still here but the panel
// no longer offers it. Discarding only stops tracking: the worktree and
// branch are yours and are never deleted here.
import { randomUUID } from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import type { Config } from "../../config";
import { AuthContext } from "../../core/atlassian";
import { runClaude } from "../../core/claude";
import {
  ClaudeSession,
  Feature,
  FeatureContext,
  FeatureDeps,
  Job,
  JobStatusError,
  WorktreeGoneError,
  jobStore,
} from "../../core/jobs";
import { IssueBasics, JiraTransition, PullRequestSummary, providersFor } from "../../core/providers";
import { askRepoChoice, confirmRepoChoice, RepoOption } from "../../core/repo-choice";
import { HeaderSegment, TerminalOptions, openClaudeCodeInTerminal } from "../../core/terminal";
import { createReviewMover } from "./review-mover";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const ticketGit = require("../../core/ticket-git.js") as {
  ensureTicketWorktree(repoPath: string, o: { issueKey: string; branch: string; base: string }): Promise<{ dir: string; branch: string; created: boolean }>;
  defaultBranchFromGit(repoPath: string): Promise<string>;
  fetchBase(dir: string, base: string): Promise<void>;
  commitsAhead(dir: string, base: string): Promise<number>;
  commitLog(dir: string, base: string): Promise<string>;
  diffStat(dir: string, base: string): Promise<string>;
  pushBranch(dir: string, branch: string): Promise<void>;
  ticketWorktreePath(repoPath: string, issueKey: string): string;
  currentBranch(dir: string): Promise<string>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const workspacePlan = require("../ticket-workspace/plan.js") as {
  prUrlAllowed(url: unknown, bitbucketBaseUrl: string): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { redactSecrets } = require("../../core/history-record.js") as { redactSecrets(text: string): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { isSafeBranchName } = require("../../core/bitbucket-endpoints.js") as { isSafeBranchName(name: unknown): boolean };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const workflow = require("../../core/jira-workflow.js") as {
  pickTransition(list: JiraTransition[], wanted: string): JiraTransition | null;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const analyzePlan = require("../analyze-issue/plan.js") as {
  readAnalysisCache(issueKey: string): {
    summary: string;
    repoKey: string | null;
    repoMatch: string | null;
    analysis: Record<string, unknown>;
  } | null;
  formatAnalysisAsMarkdownComment(o: {
    issueKey: string;
    summary: string;
    repoKey: string | null;
    repoMatch: string | null;
    analysis: Record<string, unknown>;
  }): string;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  DEFAULT_REVIEW_TRANSITION: string;
  branchNameFor(issueKey: string, summary: string | null): string;
  parseTicketToPrPayload(
    payload: unknown,
    repoKeys: string[],
  ): { issueKey: string; adopt: boolean; repoKey: string | null; hintRepoKeys: string[] };
  chooseStartFixRepo(o: {
    issue: unknown;
    repos: Record<string, string>;
    componentRepoMap: Record<string, string> | undefined;
    hintRepoKeys: string[];
  }): { proposal: string | null; proposalReason: string | null; options: RepoOption[]; ask: boolean };
  buildStartFixPrompt(o: { issueKey: string; summary: string | null; branch: string; base: string; analysisMarkdown: string | null }): string;
  fallbackDraft(o: { issueKey: string; summary: string | null; commitLog: string; jiraUrl: string }): PrDraft;
  buildDraftPrompt(o: { issueKey: string; summary: string | null; commitLog: string; diffStat: string }): string;
  parseDraft(text: string): PrDraft | null;
  validatePrForm(body: unknown): PrDraft;
  descriptionWithTicket(description: string, issueKey: string, jiraUrl: string): string;
  runCreatePrSteps(input: unknown, deps: unknown): Promise<{ ok: boolean; steps: Step[]; pr: PrRef | null }>;
  summarizeSteps(steps: Step[]): string;
};

interface PrDraft {
  title: string;
  description: string;
}
interface Step {
  id: string;
  status: string;
  detail: string;
}
interface PrRef {
  id: number;
  url: string | null;
  title?: string | null;
}

/** job.data for this feature. `ticketWorktree`, not `worktree`: that name
 * is reserved for per-job worktrees under stateDir that discard and the
 * stale-job sweep delete (core/job-files.js). This one is never deleted. */
interface TicketToPrData {
  issueKey: string;
  adopt: boolean;
  summary?: string | null;
  repoKey?: string;
  repoPath?: string;
  branch?: string;
  base?: string;
  ticketWorktree?: { dir: string };
  fixStartedAt?: number;
  claudeSession?: ClaudeSession;
  prDraft?: PrDraft & { by: "claude" | "fallback" };
  pr?: PrRef;
  steps?: Step[];
  /** Moving the ticket to review (review-mover.ts). */
  reviewMove?: { moved: boolean; status: string; detail: string; at: number };
}

const DRAFT_TIMEOUT_MS = 3 * 60 * 1000;

type CachedAnalysis = ReturnType<typeof analyzePlan.readAnalysisCache>;

/** Start fix's I/O besides git, injectable so a test runs it with no
 * network, no credentials and no terminal (features/ticket-to-pr/start-fix.test.js). */
export interface StartFixIo {
  readAnalysis(issueKey: string): CachedAnalysis;
  issueBasics(config: Config, auth: AuthContext, issueKey: string): Promise<IssueBasics>;
  /** The ticket's raw fields the repo guess reads (components, labels, links). */
  issueForRepoGuess(config: Config, auth: AuthContext, issueKey: string): Promise<unknown>;
  defaultBranch(config: Config, auth: AuthContext, project: string, repo: string): Promise<string | null>;
  openTerminal(cwd: string, args: string[], header: HeaderSegment[][], options: TerminalOptions): Promise<unknown>;
}

export const REAL_START_FIX_IO: StartFixIo = {
  readAnalysis: (issueKey) => analyzePlan.readAnalysisCache(issueKey),
  issueBasics: (config, auth, issueKey) => providersFor(config).issues.getIssueBasics(auth, issueKey),
  issueForRepoGuess: (config, auth, issueKey) =>
    providersFor(config).issues.getIssueRaw(auth, issueKey, "summary,description,components,labels,issuelinks"),
  defaultBranch: (config, auth, project, repo) => providersFor(config).git.getDefaultBranch(auth, project, repo),
  openTerminal: (cwd, args, header, options) => openClaudeCodeInTerminal(cwd, args, header, options),
};

/** A pull request reference whose address is only kept if it's a PR page on
 * the configured Bitbucket (a job file and the Bitbucket answer are untrusted). */
export function safePrRef<T extends { url?: string | null }>(pr: T | null | undefined, config: Config): T | null {
  if (!pr) return null;
  const ok = typeof pr.url === "string" && workspacePlan.prUrlAllowed(pr.url, providersFor(config).git.baseUrl());
  return { ...pr, url: ok ? (pr.url as string) : null };
}

/** safePrRef, but a Bitbucket setting that can't be read means "no address"
 * rather than a throw (this runs on every read of the job). */
function safeOrNull<T extends { url?: string | null }>(pr: T | null | undefined, config: Config): T | null {
  try {
    return safePrRef(pr, config);
  } catch {
    return pr ? { ...pr, url: null } : null;
  }
}

function dataOf(job: Job): TicketToPrData {
  return job.data as unknown as TicketToPrData;
}

function jiraBrowseUrl(config: Config, issueKey: string): string {
  return providersFor(config).issues.issueUrl(issueKey);
}

function splitRepoKey(repoKey: string): { project: string; repo: string } {
  const [project, repo] = repoKey.split("/");
  return { project, repo };
}

/** The ready worktree of an awaiting-approval job, or throws for the panel.
 * The folder is recomputed from config and must match: a job file on disk
 * is untrusted, and this is where git pushes from and a terminal opens. */
function readyWorktree(job: Job, config: Config): { d: TicketToPrData; dir: string; branch: string; base: string } {
  if (job.status !== "awaiting-approval") throw new JobStatusError(`This job is "${job.status}", not waiting for a pull request.`);
  const d = dataOf(job);
  const dir = d.ticketWorktree?.dir;
  if (!dir || !d.branch || !d.base || !d.repoKey) throw new JobStatusError("This job has no worktree yet.");
  const repoPath = config.repos?.[d.repoKey];
  const expected = repoPath ? ticketGit.ticketWorktreePath(repoPath, d.issueKey) : null;
  if (!expected || path.resolve(expected) !== path.resolve(dir)) {
    throw new JobStatusError("This job's worktree isn't where Start fix puts it — start again from the ticket.");
  }
  if (!fs.existsSync(dir)) throw new WorktreeGoneError(`The worktree ${dir} no longer exists.`);
  return { d, dir, branch: d.branch, base: d.base };
}

/** readyWorktree, plus: the worktree is on the job's branch (so the commit
 * count, the diff and the push all talk about the same branch). */
async function checkedWorktree(job: Job, config: Config) {
  const w = readyWorktree(job, config);
  const head = await ticketGit.currentBranch(w.dir);
  if (head !== w.branch) {
    throw new JobStatusError(
      `The worktree ${w.dir} is on "${head}", not ${w.branch}. Check out ${w.branch} there (no detached HEAD) and try again.`,
    );
  }
  return w;
}

async function runStartFix(
  jobId: string,
  p: { issueKey: string; adopt: boolean; repoKey: string | null; hintRepoKeys?: string[] },
  config: Config,
  deps: FeatureDeps,
  auth: AuthContext,
  io: StartFixIo,
): Promise<void> {
  const progress = (stepId: string, label: string) => jobStore.setProgressIfRunning(jobId, stepId, label);
  const { issueKey } = p;

  progress("analysis", "Reading the saved analysis…");
  const cached = io.readAnalysis(issueKey);
  let repoKey = p.repoKey || cached?.repoKey || null;
  if (!repoKey || !config.repos?.[repoKey]) {
    repoKey = null;
    const repoKeys = Object.keys(config.repos || {});
    if (repoKeys.length === 0) throw new Error("No repositories are set up yet — add one in ⚙ Settings, then start the fix again.");
    // No analysis named a repository (a story, or a project Analyze doesn't
    // cover): guess from the ticket and the workspace scan, then confirm.
    progress("repo", "Identifying the repository…");
    const issue = await io.issueForRepoGuess(config, auth, issueKey);
    const guess = plan.chooseStartFixRepo({
      issue,
      repos: config.repos || {},
      componentRepoMap: config.analyzeIssue?.componentRepoMap,
      hintRepoKeys: p.hintRepoKeys || [],
    });
    if (guess.ask) {
      progress("repo", "Waiting for you to pick the repository…");
      repoKey = await askRepoChoice(
        jobId,
        undefined,
        {
          suggested: guess.proposal,
          options: guess.options,
          prompt: guess.proposal
            ? `Start the fix for ${issueKey} in ${guess.proposal}? Nothing is checked out until you answer.`
            : `Which repository does ${issueKey} belong to? Nothing is checked out until you answer.`,
          noRepoLabel: "Cancel",
        },
        "start the fix again.",
      );
      if (!repoKey) throw new Error("Start fix was cancelled: no repository was chosen.");
    } else {
      repoKey = guess.proposal;
    }
    if (!repoKey || !config.repos?.[repoKey]) throw new Error(`No repository could be chosen for ${issueKey}.`);
  }
  const repoPath = config.repos[repoKey];
  if (!fs.existsSync(path.join(repoPath, ".git"))) throw new Error(`repos["${repoKey}"] (${repoPath}) isn't a git clone.`);

  progress("ticket", "Reading the ticket…");
  const ticket = await io.issueBasics(config, auth, issueKey);
  const summary = ticket.summary || cached?.summary || null;
  const branch = plan.branchNameFor(issueKey, summary);

  progress("base", "Finding the default branch…");
  const { project, repo } = splitRepoKey(repoKey);
  let base: string | null = null;
  try {
    base = await io.defaultBranch(config, auth, project, repo);
  } catch (err) {
    console.warn(`[ticket-to-pr] Bitbucket default branch lookup failed, using git's: ${(err as Error).message}`);
  }
  if (base && !isSafeBranchName(base)) {
    console.warn("[ticket-to-pr] Bitbucket gave an unusable default branch name, using git's");
    base = null;
  }
  if (!base) base = await ticketGit.defaultBranchFromGit(repoPath);
  if (!isSafeBranchName(base)) throw new Error("The default branch name isn't one Start fix can use safely.");

  progress("worktree", "Preparing the worktree…");
  const wt = await ticketGit.ensureTicketWorktree(repoPath, { issueKey, branch, base });
  // A reused worktree comes back before ensureTicketWorktree's own checks.
  if (!isSafeBranchName(wt.branch)) {
    throw new Error(`The worktree ${wt.dir} is on an unsafe branch name; check out a normal branch there and try again.`);
  }

  // An adopted worktree keeps the Start fix time the history recorded for it.
  let fixStartedAt: number | undefined = p.adopt ? undefined : Date.now();
  if (p.adopt) {
    const recorded = deps.history?.getItem(`worktree:${wt.dir}`)?.item.data as { fixStartedAt?: unknown } | undefined;
    if (typeof recorded?.fixStartedAt === "number") fixStartedAt = recorded.fixStartedAt;
  }
  jobStore.patchData(jobId, {
    summary,
    repoKey,
    repoPath,
    branch: wt.branch,
    base,
    ticketWorktree: { dir: wt.dir },
    ...(fixStartedAt !== undefined ? { fixStartedAt } : {}),
  });

  if (!p.adopt) {
    progress("claude", "Opening Claude Code in plan mode…");
    const analysisMarkdown = cached
      ? analyzePlan.formatAnalysisAsMarkdownComment({
          issueKey,
          summary: cached.summary,
          repoKey: cached.repoKey,
          repoMatch: cached.repoMatch,
          analysis: cached.analysis,
        })
      : null;
    const prompt = plan.buildStartFixPrompt({ issueKey, summary, branch: wt.branch, base, analysisMarkdown });
    const session: ClaudeSession = { id: randomUUID(), cwd: wt.dir, permissionMode: "plan" };
    jobStore.patchData(jobId, { claudeSession: session });
    const header: HeaderSegment[][] = [
      [{ text: "Ticket to PR", style: "title" }, { text: `  ${issueKey}`, style: "heading" }],
      [{ text: "Branch  ", style: "label" }, { text: wt.branch, style: "branch" }, { text: `  from origin/${base}`, style: "muted" }],
      [{ text: "Folder  ", style: "label" }, { text: wt.dir, style: "path" }],
      [{ text: "Commit, push and open the pull request from here; the ticket moves to review by itself.", style: "status" }],
      [],
    ];
    // Every argv word is quoted by core/terminal.ts; the prompt is one word.
    await io.openTerminal(wt.dir, ["--session-id", session.id, "--permission-mode", "plan", prompt], header, {
      title: `${issueKey} fix`,
    });
  }

  jobStore.update(jobId, {
    status: "awaiting-approval",
    progress: undefined,
    result: {
      summary: p.adopt
        ? `Tracking ${wt.branch}: the ticket moves to review once its pull request is open.`
        : `${wt.created ? "Created" : "Reused"} ${wt.dir} on ${wt.branch} and opened Claude Code in plan mode. Commit, push and open the pull request there.`,
      files: [],
    },
  });
}

export function createTicketToPrFeature(
  config: Config,
  deps: FeatureDeps = { providers: providersFor(config) },
  io: StartFixIo = REAL_START_FIX_IO,
): Feature {
  // Jobs whose Draft with Claude run is in flight (a second click is refused).
  const drafting = new Set<string>();
  // Moves the ticket to its review status once a pull request is open.
  const mover = createReviewMover(config);
  return {
    id: "ticket-to-pr",
    label: "Ticket to PR",

    startBackground: () => mover.start(),

    // A persisted job file is untrusted: the PR address the page will open
    // is re-checked on every read.
    present(job: Job): Job {
      const pr = (job.data as { pr?: PrRef } | undefined)?.pr;
      if (!pr) return job;
      return { ...job, data: { ...job.data, pr: safeOrNull(pr, config) } };
    },

    async start(payloadRaw: unknown, ctx: FeatureContext): Promise<Job> {
      const p = plan.parseTicketToPrPayload(payloadRaw, Object.keys(config.repos || {}));
      const job = jobStore.create("ticket-to-pr", { issueKey: p.issueKey, adopt: p.adopt });
      void runStartFix(job.id, p, config, deps, ctx.auth, io).catch((err: Error) =>
        jobStore.update(job.id, { status: "failed", progress: undefined, error: redactSecrets(err?.message || String(err)) }),
      );
      return job;
    },

    // Create PR. Body: { title, description } from the panel's form.
    async approve(job: Job, ctx: FeatureContext): Promise<void> {
      const { d, dir, branch, base } = readyWorktree(job, config);
      const form = plan.validatePrForm(ctx.body);
      // Claim the job before any await, so a second click is refused by the
      // status check above instead of racing this one.
      jobStore.update(job.id, { status: "approving", error: undefined, progress: { stepId: "check", label: `Checking origin/${base}…` } });
      // Anything below can throw (a malformed Bitbucket/Jira setting, a
      // credential read, git, the steps): the job must never be left in
      // "approving", which nothing can leave.
      try {
        const head = await ticketGit.currentBranch(dir);
        if (head !== branch) {
          throw new JobStatusError(
            `The worktree ${dir} is on "${head}", not ${branch}. Check out ${branch} there (no detached HEAD) and try again.`,
          );
        }
        // Refresh origin/<base> first: a stale ref would misreport the commits ahead.
        await ticketGit.fetchBase(dir, base);
        if ((await ticketGit.commitsAhead(dir, base)) === 0) {
          throw new JobStatusError(`${branch} has no commits ahead of origin/${base} yet — commit your fix first.`);
        }
        const { project, repo } = splitRepoKey(d.repoKey as string);
        const { git, issues } = deps.providers ?? providersFor(config);
        const jiraUrl = jiraBrowseUrl(config, d.issueKey);
        const description = plan.descriptionWithTicket(form.description, d.issueKey, jiraUrl);
        jobStore.update(job.id, { progress: { stepId: "pr", label: "Opening the pull request…" } });

        const outcome = await plan.runCreatePrSteps(
          {
            issueKey: d.issueKey,
            branch,
            base,
            title: form.title,
            description,
            transitionName: config.ticketToPr?.reviewTransitionName || plan.DEFAULT_REVIEW_TRANSITION,
            existingPr: safePrRef(d.pr, config),
          },
          {
            push: () => ticketGit.pushBranch(dir, branch),
            findOpenPr: async () =>
              (await git.listBranchPullRequests(ctx.auth, project, repo, branch)).find((pr) => pr.state === "OPEN") || null,
            repositoryId: () => git.getRepositoryId(ctx.auth, project, repo),
            me: () => git.whoAmI(ctx.auth),
            defaultReviewers: (repoId: number) =>
              git.getDefaultReviewers(ctx.auth, project, repo, {
                sourceRepoId: repoId,
                targetRepoId: repoId,
                sourceBranch: branch,
                targetBranch: base,
              }),
            createPr: (a: { title: string; description: string; reviewers: string[] }) =>
              git.createPullRequest(ctx.auth, { project, repo, fromBranch: branch, toBranch: base, ...a }),
            addRemoteLink: (pr: PullRequestSummary) => {
              const safe = safePrRef(pr, config);
              if (!safe?.url) throw new Error("Bitbucket didn't give a pull request address on the configured Bitbucket.");
              return issues.addRemoteLink(ctx.auth, d.issueKey, { url: safe.url, title: `PR #${pr.id}: ${pr.title || form.title}` });
            },
            listTransitions: () => issues.listTransitions(ctx.auth, d.issueKey),
            pickTransition: workflow.pickTransition,
            transition: (id: string) => issues.transitionIssue(ctx.auth, d.issueKey, id),
          },
        );

        const report = plan.summarizeSteps(outcome.steps);
        const pr = outcome.pr ? safePrRef({ id: outcome.pr.id, url: outcome.pr.url, title: outcome.pr.title ?? form.title }, config) : undefined;
        // Data first, then the status: the history records the job as its status changes.
        jobStore.patchData(job.id, { steps: outcome.steps, ...(pr ? { pr } : {}), prDraft: { ...form, by: d.prDraft?.by || "fallback" } });
        if (!outcome.ok) throw new Error(redactSecrets(report)); // the catch below puts the job back
        jobStore.update(job.id, {
          status: "approved",
          progress: undefined,
          result: { summary: pr ? `Opened pull request #${pr.id} for ${d.issueKey}.\n${report}` : report, files: [] },
        });
      } catch (err) {
        jobStore.update(job.id, {
          status: "awaiting-approval",
          progress: undefined,
          error: redactSecrets((err as Error)?.message || String(err)),
        });
        throw err;
      }
    },

    // Stop tracking. The worktree and branch stay where they are.
    async reject(job: Job): Promise<void> {
      if (job.status !== "awaiting-approval" && job.status !== "failed") {
        throw new JobStatusError(`Cannot stop tracking a job that is "${job.status}".`);
      }
      jobStore.update(job.id, { status: "rejected", progress: undefined });
    },

    actions: {
      // Body { repoKey }: one of the repos offered in job.data.pendingChoice,
      // or null to cancel the fix.
      async "confirm-repo"(job: Job, ctx: FeatureContext): Promise<unknown> {
        return confirmRepoChoice(job, ctx.body);
      },

      // Moves the ticket to its review status now (the panel's button); the
      // poll does the same by itself once a pull request from the branch is open.
      async "move-to-review"(job: Job, ctx: FeatureContext): Promise<Job | undefined> {
        if (job.status !== "awaiting-approval") throw new JobStatusError(`This job is "${job.status}", not waiting for a pull request.`);
        const outcome = await mover.moveNow(job, ctx.auth);
        if (outcome.status === "failed" || (outcome.status === "already" && !dataOf(job).reviewMove)) {
          throw new JobStatusError(outcome.detail);
        }
        return jobStore.get(job.id);
      },
      // A title and description from a Claude run with no tools, over the
      // branch's commits; falls back to a plain draft if that fails.
      async "draft-pr"(job: Job): Promise<Job> {
        // Single flight: one Claude run per job at a time.
        if (drafting.has(job.id)) throw new JobStatusError("A draft is already being written for this job — wait for it.");
        drafting.add(job.id);
        try {
          return await draftPr(job);
        } finally {
          drafting.delete(job.id);
        }
      },
      // A fresh Claude Code in the worktree (Continue in Claude Code covers
      // the recorded session; this is for an adopted worktree without one).
      async "open-terminal"(job: Job): Promise<unknown> {
        const { dir } = readyWorktree(job, config);
        await openClaudeCodeInTerminal(dir);
        return { ok: true };
      },
    },
  };

  async function draftPr(job: Job): Promise<Job> {
    const { d, dir, base } = await checkedWorktree(job, config);
    // A stale origin/<base> would misreport what the branch adds.
    await ticketGit.fetchBase(dir, base);
    if ((await ticketGit.commitsAhead(dir, base)) === 0) {
      throw new JobStatusError(`${d.branch} has no commits ahead of origin/${base} yet — commit your fix first.`);
    }
    const commitLog = await ticketGit.commitLog(dir, base);
    const diffStat = await ticketGit.diffStat(dir, base);
    const fallback = plan.fallbackDraft({ issueKey: d.issueKey, summary: d.summary || null, commitLog, jiraUrl: jiraBrowseUrl(config, d.issueKey) });
    let draft: PrDraft & { by: "claude" | "fallback" } = { ...fallback, by: "fallback" };
    const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "ticket-to-pr-draft-"));
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(new Error("The draft took too long.")), DRAFT_TIMEOUT_MS);
    try {
      const result = await runClaude({
        cwd: scratch,
        prompt: plan.buildDraftPrompt({ issueKey: d.issueKey, summary: d.summary || null, commitLog, diffStat }),
        policy: "noTools",
        signal: controller.signal,
      });
      const parsed = plan.parseDraft(result.text);
      if (parsed) draft = { ...parsed, by: "claude" };
    } catch (err) {
      console.warn(`[ticket-to-pr] draft failed, using the plain draft: ${(err as Error).message}`);
    } finally {
      clearTimeout(timer);
      fs.rmSync(scratch, { recursive: true, force: true });
    }
    return jobStore.patchData(job.id, { prDraft: draft });
  }
}
