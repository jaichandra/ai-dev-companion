import { randomUUID } from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { saveComponentRepo } from "../../config";
import type { Config } from "../../config";
import { AuthContext } from "../../core/atlassian";
import { runClaude } from "../../core/claude";
import { ClaudeSession, Feature, FeatureContext, FeatureDeps, FindSimilar, Job, JobStatusError, McpToolContext, SimilarEntry, jobStore } from "../../core/jobs";
import { providersFor } from "../../core/providers";
import { RepoChoiceCancelledError as JobCancelledError, RepoOption, askRepoChoice, confirmRepoChoice } from "../../core/repo-choice";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const targets = require("../../core/targets.js") as {
  analyzeTargetsFrom(config: Config): { projects: string[]; issueTypes: string[] };
  describeAnalyzeScope(t: { projects: string[]; issueTypes: string[] }): string;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("../../core/paths.js") as { stateDir(home?: string): string };

// eslint-disable-next-line @typescript-eslint/no-var-requires
const analysisCheckout = require("../../core/analysis-checkout.js") as {
  ensureAnalysisCheckout(opts: { userClone: string; repoKey: string; root: string }): Promise<{ dir: string; branch: string; fresh: boolean }>;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const packs = require("../../core/packs.js") as {
  analysisServerHints(): { server: string; whenAvailable: string; whenMissing: string }[];
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  MUTATING_MCP_TOOLS: string[];
  parseIssueKey(raw: string): { key: string; projectKey: string };
  extractUrls(issue: unknown): string[];
  candidateReposFromTicket(
    issue: unknown,
    reposMap: Record<string, string>,
    componentRepoMap: Record<string, string> | undefined,
    linkedPrs?: LinkedPr[],
  ): Array<{ repoKey: string; score: number; reason: string }>;
  isStrongRepoMatch(candidates: Array<{ repoKey: string; score: number; reason: string }>): boolean;
  buildRepoPickPrompt(issue: unknown, repoKeys: string[], hintKeys?: string[], linkedPrs?: LinkedPr[]): string;
  COMMENT_SUMMARY_MODEL: string;
  buildCommentSummaryPrompt(fullMarkdown: string): string;
  assembleSummaryComment(opts: { issueKey?: string; summaryText: string }): string;
  linkedPullRequests(issue: unknown, devStatus: unknown, bitbucketBaseUrl: string): LinkedPr[];
  isSettledRepoMatch(issue: unknown, proposal: string | null, componentRepoMap: Record<string, string> | undefined): boolean;
  repoChoiceOptions(opts: {
    proposal: string | null;
    proposalReason: string | null;
    candidates: Array<{ repoKey: string; reason: string }>;
    hintKeys: string[];
    repoKeys: string[];
    linkedPrs?: LinkedPr[];
  }): RepoOption[];
  confirmedReason(choice: string | null, proposal: string | null, proposalReason: string | null): string;
  repoHintsFromSimilar(similarItems: SimilarEntry[], repoKeys: string[]): string[];
  componentMappingsToLearn(
    issue: unknown,
    repoKey: string | null,
    reason: string | null,
    componentRepoMap: Record<string, string> | undefined,
  ): Record<string, string>;
  parseRepoPick(text: string, repoKeys: string[]): string | null;
  buildAnalysisPrompt(opts: {
    issue: unknown;
    urls: string[];
    repoKey: string | null;
    mcpServers: string[];
    serverHints?: { server: string; available: boolean; line: string }[];
    background?: boolean;
    similar?: SimilarEntry[];
  }): string;
  similarQueryText(issue: unknown): string;
  fetchSimilarTickets(
    findSimilar: FindSimilar | undefined,
    query: { key: string; text: string },
    opts?: { timeoutMs?: number; warn?: (message: string) => void },
  ): Promise<SimilarEntry[]>;
  analysisRunOptions(
    background: boolean,
    readMcpServers: () => string[],
  ): { policy: "readOnly" | "readOnlyBackground"; mcpServers: string[]; extraAllowedTools: string[]; useUrls: boolean };
  parseAnalysis(text: string): Record<string, unknown>;
  readMcpServers(claudeJsonPath?: string): string[];
  buildSummary(opts: {
    issueKey: string;
    repoKey: string | null;
    analysis: Record<string, unknown>;
  }): string;
  formatAnalysisAsMarkdownComment(opts: {
    issueKey?: string;
    summary?: string;
    repoKey?: string | null;
    repoMatch?: string | null;
    analysis: Record<string, unknown>;
  }): string;
  markdownToJiraWiki(md: string): string;
  readAnalysisCache(issueKey: string, rootDir?: string): {
    issueKey: string;
    summary: string;
    repoKey: string | null;
    repoMatch: string | null;
    analysis: Record<string, unknown>;
    completedAt: string;
    claudeSession?: ClaudeSession;
  } | null;
  writeAnalysisCache(
    entry: {
      issueKey: string;
      summary: string;
      repoKey: string | null;
      repoMatch: string | null;
      analysis: Record<string, unknown>;
      completedAt: string;
      claudeSession?: ClaudeSession;
    },
    rootDir?: string,
  ): void;
};

interface JiraIssue {
  id?: string;
  key?: string;
  fields?: {
    summary?: string;
    description?: unknown;
    issuetype?: { name?: string };
    project?: { key?: string };
    components?: Array<{ name?: string }>;
    labels?: string[];
    priority?: { name?: string };
    versions?: Array<{ name?: string }>;
    fixVersions?: Array<{ name?: string }>;
    environment?: unknown;
    comment?: { comments?: Array<{ body?: unknown; author?: { displayName?: string; name?: string } }> };
  };
  remoteLinks?: unknown[];
}

/** A pull request already linked to the ticket (plan.linkedPullRequests). */
interface LinkedPr {
  repoKey: string;
  project: string;
  repo: string;
  id: number;
  title: string | null;
  state: string | null;
  url: string | null;
}

/** Running jobs by id: aborting the controller kills the current Claude
 * step (see core/claude.ts + core/exec.ts). `done` settles once runAnalyze
 * has stopped updating the job. */
const running = new Map<string, { controller: AbortController; done: Promise<void> }>();


/** Markdown draft stamped on job.data for the extension's compose-before-post UI. */
function commentDraftFromData(data: {
  issueKey: string;
  summary?: string;
  repoKey?: string | null;
  repoMatch?: string | null;
  analysis: Record<string, unknown>;
}): string {
  return plan.formatAnalysisAsMarkdownComment({
    issueKey: data.issueKey,
    summary: data.summary || "",
    repoKey: data.repoKey ?? null,
    repoMatch: data.repoMatch ?? null,
    analysis: data.analysis,
  });
}

export function createAnalyzeIssueFeature(config: Config, deps: FeatureDeps = { providers: providersFor(config) }): Feature {
  return {
    id: "analyze-issue",
    label: "Analyze ticket",

    // Polled read-only shape (same as pre-deployment-stats): /start
    // returns immediately; the job finishes in awaiting-approval with
    // data.analysis so the extension auto-opens a Done-only panel.
    async start(payloadRaw: unknown, ctx: FeatureContext): Promise<Job> {
      const p = payloadRaw as { issueKey?: unknown; force?: unknown; confirmRepo?: unknown } | null;
      if (!p || typeof p.issueKey !== "string") {
        throw new Error("analyze-issue payload must include issueKey");
      }
      const { key: issueKey } = plan.parseIssueKey(p.issueKey);
      const force = p.force === true;
      // Terminal Claude's analyze_issue tool sends confirmRepo: false — there is
      // nobody at the browser to answer, so it takes the best match as before.
      const confirmRepo = p.confirmRepo !== false;
      // A watcher's run (ctx.background, set only by the service itself):
      // the stricter readOnlyBackground policy and no MCP servers.
      const background = ctx.background === true;
      const job = jobStore.create("analyze-issue", { issueKey });
      const controller = new AbortController();

      // Deliberately not awaited — the HTTP handler returns the (running)
      // job immediately and the extension polls /status (same pattern as
      // resolve-conflict / pre-deployment-stats). force: true skips the
      // on-disk cache and always re-runs Claude (Re-analyze).
      const work = force
        ? runAnalyze(job.id, issueKey, config, ctx.auth, controller.signal, background, deps.findSimilar, confirmRepo)
        : loadCachedOrAnalyze(job.id, issueKey, config, ctx.auth, controller.signal, background, deps.findSimilar, confirmRepo);
      const done = work
        .catch((err: Error) => {
          if (!controller.signal.aborted) {
            jobStore.update(job.id, {
              status: "failed",
              error: err?.message || String(err),
            });
            return;
          }
          // Cancel already flipped the job to "rejecting"; land on rejected
          // with nothing to clean up (no worktree / no pushed state).
          jobStore.update(job.id, { status: "rejected", progress: undefined });
        })
        .finally(() => running.delete(job.id));
      running.set(job.id, { controller, done });

      return job;
    },

    async cancel(job: Job): Promise<void> {
      const entry = running.get(job.id);
      if (!entry || job.status !== "running") {
        throw new Error(`Cannot cancel job in status "${job.status}"`);
      }
      jobStore.update(job.id, {
        status: "rejecting",
        progress: { stepId: "cancel", label: "Cancelling analysis…" },
      });
      entry.controller.abort(new JobCancelledError("cancelled"));
      await entry.done;
    },

    async approve(job: Job): Promise<void> {
      throw new Error(`Cannot approve job in status "${job.status}" — analyze-issue is a read-only report`);
    },

    async reject(job: Job): Promise<void> {
      // Cancel-after-finish race on POST .../cancel: discard the report.
      // There is nothing to clean up (unlike resolve-conflict's worktree).
      if (job.status === "awaiting-approval" || job.status === "failed") {
        jobStore.update(job.id, { status: "rejected", progress: undefined });
        return;
      }
      throw new Error(`Cannot reject job in status "${job.status}" — analyze-issue is a read-only report`);
    },

    // POST /features/analyze-issue/:jobId/post-comment — body must include
    // { body: string } (Markdown from the compose textarea). Converted to
    // Jira Server wiki markup and posted via authedJson (browser SSO first).
    // Does not change job status: the panel stays open for Done / Re-analyze.
    async postComment(job: Job, ctx: FeatureContext): Promise<{ commentId?: string }> {
      if (job.status !== "awaiting-approval") {
        throw new Error(`Cannot post a comment for a job in status "${job.status}"`);
      }
      const data = job.data || {};
      const issueKey = typeof data.issueKey === "string" ? data.issueKey : "";
      if (!issueKey) throw new Error("No issue key on this analysis job");

      const reqBody = ctx.body as { body?: unknown } | null | undefined;
      if (!reqBody || typeof reqBody.body !== "string") {
        throw new Error('post-comment requires a Markdown "body" string');
      }
      const markdown = reqBody.body;
      if (!markdown.trim()) throw new Error("Comment body is empty");

      const wikiBody = plan.markdownToJiraWiki(markdown);
      if (!wikiBody.trim()) throw new Error("Formatted comment was empty");

      const created = await (deps.providers ?? providersFor(config)).issues.addComment(ctx.auth, issueKey, wikiBody);

      return { commentId: created.id || undefined };
    },

    actions: {
      // Body { repoKey }: one of the repos offered in job.data.pendingChoice,
      // or null to analyze without a repo.
      async "confirm-repo"(job: Job, ctx: FeatureContext): Promise<unknown> {
        return confirmRepoChoice(job, ctx.body);
      },

      // "Add as comment": the full analysis boiled down to a short draft
      // (under 250 words) by Haiku. The panel puts it in the compose box
      // for the user to edit; the full draft stays on job.data as fallback.
      async "summarize-comment"(job: Job): Promise<{ draft: string }> {
        if (job.status !== "awaiting-approval") {
          throw new Error(`Cannot summarize a comment for a job in status "${job.status}"`);
        }
        const full = job.data?.commentDraftMd;
        if (typeof full !== "string" || !full.trim()) throw new Error("This job has no analysis to summarize");
        const issueKey = typeof job.data?.issueKey === "string" ? job.data.issueKey : "";

        const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-comment-"));
        try {
          const result = await runClaude({
            cwd,
            prompt: plan.buildCommentSummaryPrompt(full),
            model: plan.COMMENT_SUMMARY_MODEL,
            policy: "noTools",
          });
          const draft = plan.assembleSummaryComment({ issueKey, summaryText: result.text });
          if (!draft) throw new Error("Claude returned an empty summary");
          return { draft };
        } finally {
          fs.rmSync(cwd, { recursive: true, force: true });
        }
      },
    },

    // Read-only both: get_issue_analysis only reads the on-disk cache and
    // the job store; analyze_issue runs the same readOnly-policy Claude
    // analysis the panel does (never postComment — posting stays a
    // browser click).
    mcpTools() {
      return [
        {
          name: "get_issue_analysis",
          async handler(args: Record<string, unknown>): Promise<unknown> {
            const issueKey = args.issueKey as string;
            const cached = plan.readAnalysisCache(issueKey);
            if (cached) {
              return {
                issueKey: cached.issueKey,
                summary: cached.summary,
                repoKey: cached.repoKey,
                analysis: cached.analysis,
                completedAt: cached.completedAt,
              };
            }
            const job = jobStore.lookup(`jira:${issueKey}`, "analyze-issue");
            if (job?.status === "running") {
              return { issueKey, jobId: job.id, status: job.status, progress: job.progress?.label };
            }
            return { found: false, hint: "Call analyze_issue to start one." };
          },
        },
        {
          name: "analyze_issue",
          async handler(args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
            const issueKey = args.issueKey as string;
            const started = await ctx.startJob("analyze-issue", { issueKey, force: args.force === true, confirmRepo: false });
            const job = await ctx.awaitJob(started.id, 55_000);
            if (job.status === "running") {
              return {
                jobId: job.id,
                status: "running",
                progress: job.progress?.label,
                hint: "Still analyzing — call get_issue_analysis (or get_job with this jobId) in a minute.",
              };
            }
            if (job.status === "failed") throw new Error(job.error || "The analysis failed.");
            const data = job.data as { issueKey?: string; summary?: string; repoKey?: string | null; analysis?: unknown };
            if (!data.analysis) throw new Error(`The analysis ended "${job.status}" without a result.`);
            return {
              jobId: job.id,
              issueKey: data.issueKey,
              summary: data.summary,
              repoKey: data.repoKey,
              analysis: data.analysis,
            };
          },
        },
      ];
    },
  };
}

/** Prefer a successful on-disk analysis; otherwise run Claude as usual. */
async function loadCachedOrAnalyze(
  jobId: string,
  issueKey: string,
  config: Config,
  auth: AuthContext,
  signal: AbortSignal,
  background = false,
  findSimilar?: FindSimilar,
  confirmRepo = true,
): Promise<void> {
  if (signal.aborted) throw new JobCancelledError("cancelled");
  const cached = plan.readAnalysisCache(issueKey);
  if (cached) {
    jobStore.update(jobId, {
      progress: { stepId: "load", label: "Loading saved analysis…" },
    });
    // Yield so the extension can paint the brief progress label before we
    // flip to awaiting-approval.
    await new Promise((r) => setTimeout(r, 50));
    if (signal.aborted) throw new JobCancelledError("cancelled");
    const commentDraftMd = commentDraftFromData({
      issueKey: cached.issueKey,
      summary: cached.summary,
      repoKey: cached.repoKey,
      repoMatch: cached.repoMatch,
      analysis: cached.analysis,
    });
    jobStore.update(jobId, {
      status: "awaiting-approval",
      progress: undefined,
      data: {
        issueKey: cached.issueKey,
        summary: cached.summary,
        repoKey: cached.repoKey,
        repoMatch: cached.repoMatch,
        analysis: cached.analysis,
        commentDraftMd,
        completedAt: cached.completedAt,
        fromCache: true,
        claudeSession: cached.claudeSession,
      },
      result: {
        summary: plan.buildSummary({
          issueKey: cached.issueKey,
          repoKey: cached.repoKey,
          analysis: cached.analysis,
        }),
        files: [],
      },
    });
    return;
  }
  await runAnalyze(jobId, issueKey, config, auth, signal, background, findSimilar, confirmRepo);
}

async function runAnalyze(
  jobId: string,
  issueKey: string,
  config: Config,
  auth: AuthContext,
  signal: AbortSignal,
  background = false,
  findSimilar?: FindSimilar,
  confirmRepo = true,
): Promise<void> {
  // Called before every status/progress update, so a cancelled job never
  // moves on to the next step or overwrites its "rejecting" state.
  const checkpoint = () => {
    if (signal.aborted) throw new JobCancelledError("cancelled");
  };

  const { issues, git } = providersFor(config);
  const model = config.analyzeIssue?.model || undefined;
  const progress = (stepId: string, label: string) => {
    checkpoint();
    jobStore.update(jobId, { progress: { stepId, label } });
  };

  progress("fetch", "Reading ticket…");
  const fields =
    "summary,description,issuetype,components,labels,priority,versions,fixVersions,environment,comment,issuelinks";
  const issue = (await issues.getIssueRaw(auth, issueKey, fields)) as JiraIssue;
  checkpoint();
  issue.key = issue.key || issueKey;

  const typeName = issue.fields?.issuetype?.name;
  const projectKey =
    issue.fields?.project?.key || plan.parseIssueKey(issueKey).projectKey;
  const wanted = targets.analyzeTargetsFrom(config);
  const scope = targets.describeAnalyzeScope(wanted);
  if (!wanted.projects.includes(projectKey.toUpperCase())) {
    throw new Error(`${issueKey} is in project ${projectKey}, not one of ${wanted.projects.join(", ")} — Analyze ticket only runs on ${scope}.`);
  }
  if (wanted.issueTypes.length > 0 && (!typeName || !wanted.issueTypes.includes(typeName))) {
    throw new Error(
      `${issueKey} is a ${typeName || "ticket"}, not one of ${wanted.issueTypes.join(", ")} — Analyze ticket only runs on ${scope}.`,
    );
  }

  let remoteLinks: unknown[] = [];
  try {
    remoteLinks = (await issues.getRemoteLinksRaw(auth, issueKey)) as unknown[];
  } catch (err) {
    console.warn(`[analyze-issue] remotelink fetch failed: ${(err as Error).message}`);
  }
  checkpoint();
  issue.remoteLinks = Array.isArray(remoteLinks) ? remoteLinks : [];

  // Pull requests already linked to the ticket, from Jira's development panel
  // (plus any PR address in its links and text). Never fatal: the panel is an
  // optional Jira add-on and may be absent or not allowed.
  let devStatus: unknown;
  if (issue.id && /^\d+$/.test(issue.id)) {
    try {
      devStatus = await issues.getLinkedPullRequestsRaw(auth, issue.id);
    } catch (err) {
      console.warn(`[analyze-issue] linked pull requests unavailable: ${(err as Error).message}`);
    }
  }
  checkpoint();
  const linkedPrs = plan.linkedPullRequests(issue, devStatus, git.baseUrl());

  // Similar past tickets from the local history (Phase 8): fenced, clipped
  // and labelled as untrusted earlier AI output by plan.buildAnalysisPrompt.
  // A background run gets them too (data, not a tool). Never fatal.
  let similarItems: SimilarEntry[] = [];
  if (findSimilar) {
    similarItems = await plan.fetchSimilarTickets(
      findSimilar,
      { key: issueKey, text: plan.similarQueryText(issue) },
      { warn: (message) => console.warn(`[analyze-issue] similar past tickets skipped: ${message}`) },
    );
    checkpoint();
  }

  // A background run can't fetch anything, so its prompt lists no links.
  const runOptions = plan.analysisRunOptions(background, () => plan.readMcpServers());
  const urls = runOptions.useUrls ? plan.extractUrls(issue) : [];

  progress("repo", "Identifying repository…");
  const candidates = plan.candidateReposFromTicket(
    issue,
    config.repos || {},
    config.analyzeIssue?.componentRepoMap,
    linkedPrs,
  );
  const repoKeys = Object.keys(config.repos || {});

  // What we would pick: a clear ticket-based match, else Claude's pick.
  let proposal: string | null = null;
  let proposalReason: string | null = null;
  if (plan.isStrongRepoMatch(candidates)) {
    proposal = candidates[0].repoKey;
    proposalReason = candidates[0].reason;
  } else if (repoKeys.length > 0) {
    progress("repo", "Asking Claude to pick a repository…");
    const pickCwd = fs.mkdtempSync(path.join(os.tmpdir(), "analyze-issue-pick-"));
    try {
      const pick = await runClaude({
        cwd: pickCwd,
        prompt: plan.buildRepoPickPrompt(issue, repoKeys, plan.repoHintsFromSimilar(similarItems, repoKeys), linkedPrs),
        model,
        policy: "noTools",
        signal,
      });
      checkpoint();
      proposal = plan.parseRepoPick(pick.text, repoKeys);
      if (proposal) proposalReason = "Claude pick";
    } finally {
      fs.rmSync(pickCwd, { recursive: true, force: true });
    }
  }

  let repoKey = proposal;
  let repoMatchReason = proposalReason;

  // The user confirms the repo before anything is checked out or analyzed,
  // unless a component mapping they made or confirmed already settles it. A
  // run nobody is watching (a watcher's, or terminal Claude's) can't ask.
  if (confirmRepo && !background && repoKeys.length > 0 && !plan.isSettledRepoMatch(issue, proposal, config.analyzeIssue?.componentRepoMap)) {
    const options = plan.repoChoiceOptions({
      proposal,
      proposalReason,
      candidates,
      hintKeys: plan.repoHintsFromSimilar(similarItems, repoKeys),
      repoKeys,
      linkedPrs,
    });
    progress("repo", "Waiting for you to confirm the repository…");
    const choice = await askRepoChoice(jobId, signal, { suggested: proposal, options }, "run Analyze ticket again.");
    checkpoint();
    repoKey = choice;
    repoMatchReason = choice ? plan.confirmedReason(choice, proposal, proposalReason) : null;
  }

  // Remember what the user settled on, so the next ticket with this component
  // needs no asking. Never fatal.
  if (repoKey) {
    const learned = plan.componentMappingsToLearn(issue, repoKey, repoMatchReason, config.analyzeIssue?.componentRepoMap);
    for (const [component, repo] of Object.entries(learned)) {
      try {
        if (saveComponentRepo(config, component, repo)) {
          console.log(`[analyze-issue] Learned component "${component}" -> ${repo} (${repoMatchReason}).`);
        }
      } catch (err) {
        console.warn(`[analyze-issue] could not save component mapping: ${err instanceof Error ? err.message : err}`);
      }
    }
  }

  // The analysis reads a read-only checkout of the latest default branch kept
  // under the state dir, not the user's own clone (whatever branch and edits it
  // has) and not a worktree folder beside it.
  let cwd: string | null = null;
  if (repoKey) {
    const userClone = config.repos[repoKey];
    if (!userClone || !fs.existsSync(path.join(userClone, ".git"))) {
      console.warn(`[analyze-issue] repos["${repoKey}"] = "${userClone}" is not a git checkout; analyzing without repo.`);
      repoKey = null;
      repoMatchReason = null;
    } else {
      progress("repo", "Getting the latest code from the repository…");
      try {
        const checkout = await analysisCheckout.ensureAnalysisCheckout({
          userClone,
          repoKey,
          root: path.join(paths.stateDir(), "repos"),
        });
        checkpoint();
        cwd = checkout.dir;
        repoMatchReason = `${repoMatchReason} · origin/${checkout.branch}${checkout.fresh ? "" : " (could not fetch — last copy)"}`;
      } catch (err) {
        checkpoint();
        console.warn(`[analyze-issue] read-only checkout of ${repoKey} failed, using your clone: ${err instanceof Error ? err.message : err}`);
        cwd = userClone;
        repoMatchReason = `${repoMatchReason} · your own clone, as it is`;
      }
    }
  }

  // MCP read tools take free-text arguments (a search is a way out), so a
  // run nobody clicked for gets none.
  const mcpServers = runOptions.mcpServers;
  const serverHints = packs.analysisServerHints().map((h) => {
    const available = mcpServers.some((s) => s.toLowerCase() === h.server.toLowerCase());
    return { server: h.server, available, line: available ? h.whenAvailable : h.whenMissing };
  });

  // Keep the live progress label short — MCP server names belong in the
  // report's toolsUsed, not the checklist step.
  progress("analyze", "Analyzing with Claude…");

  // No matched repo: keep a per-issue directory under the state dir rather
  // than the old mkdtemp-then-delete temp dir, so claudeSession's cwd is
  // still there for a later phase to --resume into (a deleted tmpdir would
  // make that resume fail).
  let analysisCwd = cwd;
  if (!analysisCwd) {
    analysisCwd = path.join(paths.stateDir(), "sessions", issueKey);
    fs.mkdirSync(analysisCwd, { recursive: true });
    if (!repoMatchReason) repoMatchReason = "no repo matched — analyzed without a local clone";
  }

  // Own session id (not the repo pick's — Claude refuses a reused
  // --session-id, and the repo pick runs in its own temp dir anyway) so a
  // later phase can --resume this exact transcript in analysisCwd.
  const sessionId = randomUUID();
  // Stamped BEFORE the run, not just after it succeeds: id/cwd/mode are
  // all already known (the id is ours, not the CLI's echoed-back one —
  // see below), so a run that fails partway through still leaves a
  // resumable session on the job, and the footer's "failed" condition
  // (see the extension feature file (chrome-extension/features/<id>.js)'s Continue in Claude Code) can
  // offer to pick it back up instead of only a full Re-analyze.
  jobStore.patchData(jobId, {
    claudeSession: { id: sessionId, cwd: analysisCwd, permissionMode: "auto" } as ClaudeSession,
  });
  const result = await runClaude({
    cwd: analysisCwd,
    prompt: plan.buildAnalysisPrompt({
      issue,
      urls,
      repoKey,
      mcpServers,
      serverHints,
      background,
      similar: similarItems,
    }),
    model,
    policy: runOptions.policy,
    extraAllowedTools: runOptions.extraAllowedTools,
    disallowedTools: plan.MUTATING_MCP_TOOLS,
    sessionId,
    signal,
    onProgress: (label) => jobStore.setProgressIfRunning(jobId, "analyze", label),
  });
  progress("format", "Preparing report…");
  const analysis: Record<string, unknown> = plan.parseAnalysis(result.text);
  const claudeSession: ClaudeSession = {
    id: result.sessionId || sessionId,
    cwd: analysisCwd,
    permissionMode: "auto",
  };

  checkpoint();

  // Flat job.data shape expected by the extension feature file (chrome-extension/features/<id>.js)
  // analyze-issue renderPanel: { issueKey, summary, repoKey, repoMatch, analysis }.
  // Also stamp toolsUsed.preferred for the hinted servers Claude left out.
  if (
    analysis &&
    typeof analysis === "object" &&
    !("raw" in analysis && analysis.raw && !analysis.tldr)
  ) {
    const toolsUsed =
      analysis.toolsUsed && typeof analysis.toolsUsed === "object"
        ? (analysis.toolsUsed as Record<string, unknown>)
        : {};
    const reported = (toolsUsed.preferred && typeof toolsUsed.preferred === "object" ? toolsUsed.preferred : {}) as Record<string, boolean>;
    if (serverHints.some((h) => reported[h.server] == null)) {
      analysis.toolsUsed = {
        ...toolsUsed,
        preferred: { ...Object.fromEntries(serverHints.map((h) => [h.server, h.available])), ...reported },
        mcpServers: Array.isArray(toolsUsed.mcpServers)
          ? toolsUsed.mcpServers
          : mcpServers,
      };
    }
  }

  const summary = issue.fields?.summary || "";
  const completedAt = new Date().toISOString();
  const commentDraftMd = commentDraftFromData({
    issueKey,
    summary,
    repoKey,
    repoMatch: repoMatchReason,
    analysis,
  });
  try {
    plan.writeAnalysisCache({
      issueKey,
      summary,
      repoKey,
      repoMatch: repoMatchReason,
      analysis,
      completedAt,
      claudeSession,
    });
  } catch (err) {
    console.warn(`[analyze-issue] cache write failed: ${(err as Error).message}`);
  }

  jobStore.update(jobId, {
    status: "awaiting-approval",
    progress: undefined,
    data: {
      issueKey,
      summary,
      repoKey,
      repoMatch: repoMatchReason,
      analysis,
      commentDraftMd,
      completedAt,
      fromCache: false,
      claudeSession,
    },
    result: {
      summary: plan.buildSummary({ issueKey, repoKey, analysis }),
      files: [],
    },
  });
}
