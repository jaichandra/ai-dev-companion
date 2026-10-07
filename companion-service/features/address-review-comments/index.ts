import { randomUUID } from "crypto";
import * as fs from "fs";
import * as path from "path";
import type { Config } from "../../config";
import { repoPath as resolveRepoPath } from "../../config";
import type { AuthContext } from "../../core/atlassian";
import { providersFor } from "../../core/providers";
import { runClaude } from "../../core/claude";
import { runCursorAgent } from "../../core/cursor-agent";
import { resolveReviewEditor } from "../../core/editor";
import { computeFileDiffs } from "../../core/diff";
import { git } from "../../core/exec";
import { ClaudeSession, Feature, Job, JobStatusError, McpToolContext, WorktreeGoneError, jobStore } from "../../core/jobs";
import { fetchOpenPullRequest, pendingStartResult } from "../../core/mcp";
import { assertTreeMatchesReviewed, canReject, stageAll, withJobLock, writeTree } from "../../core/reviewed-push";
import { addWorktree, removeWorktree, WorktreeHandle } from "../../core/worktree";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const { baselines } = require("../../core/integrity-baselines.js") as {
  baselines: {
    take(jobId: string, opts: { worktreeDir: string; gitDir: string }): unknown;
    check(jobId: string, worktreeDir: string): "intact" | "changed" | "missing";
    remove(jobId: string): void;
  };
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const { isProcessGroupAlive } = require("../../core/process-group.js") as {
  isProcessGroupAlive(pgid: unknown): boolean;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const claudeArgs = require("../../core/claude-args.js") as {
  normalizeCheckCommands(checkCommands: unknown): string[];
  checkCommandRules(checkCommands: unknown): string[];
};

/** core/bitbucket-normalize.js's ReviewComment. */
interface ReviewComment {
  id: number;
  version: number | null;
  text: string | null;
  authorSlug: string | null;
  authorName: string | null;
  createdAt: number | null;
  severity: string;
  state: string;
  threadResolved: boolean | null;
  anchor: {
    path: string | null;
    line: number | null;
    lineType: string | null;
    fileType: string | null;
    diffType: string | null;
    orphaned: boolean;
  } | null;
  replies: ReviewComment[];
}

/** The subset of core/bitbucket-normalize.js's normalizePullRequest this
 * feature reads. */
interface PullRequestInfo {
  id: number;
  title: string | null;
  state: string | null;
  fromBranch: string | null;
  fromSha: string | null;
  fromRepo: { projectKey: string | null; slug: string | null } | null;
  toBranch: string | null;
  toRepo: { projectKey: string | null; slug: string | null } | null;
}

type ReportAction = "fixed" | "declined" | "needs-discussion";
interface ReportEntry {
  commentId: number;
  action: ReportAction;
  note: string;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  validatePayload(body: unknown): Payload;
  collectOpenComments(comments: ReviewComment[], me: string | null): ReviewComment[];
  buildAddressCommentsPrompt(
    comments: ReviewComment[],
    pr: { id: number; title?: string | null; fromBranch?: string | null; toBranch?: string | null },
    opts?: { checkCommands?: string[] },
  ): string;
  parseAddressReport(text: string, commentIds: number[]): ReportEntry[] | null;
  defaultReplies(report: ReportEntry[] | null): Array<{ commentId: number; action: ReportAction; text: string }>;
  selectReplies(replies: unknown, comments: ReviewComment[]): Array<{ commentId: number; text: string }>;
  commitMessage(pr: { id: number }, report: ReportEntry[] | null): string;
  approveGitSteps(opts: {
    stagedClean: boolean;
    head: string;
    headParent: string | null;
    preSha: string;
    approveCommit: string | undefined;
  }): { error?: string; resetTo?: string | null; commit?: boolean; push?: boolean };
  commitTreeError(headTree: string, reviewedTree: string | undefined): string | null;
  pullRequestRepoError(pr: PullRequestInfo, payload: Payload): string | null;
  sourceShaError(preSha: string, fromSha: string | null): string | null;
  prePushError(opts: {
    headParent: string | null;
    headTree: string;
    preSha: string;
    reviewedTree: string | undefined;
  }): string | null;
  approveSummary(opts: {
    pushed: boolean;
    branch: string;
    posted: number[];
    failed: Array<{ commentId: number; error: string }>;
  }): string;
};

interface Payload {
  project: string;
  repo: string;
  prId: number;
}

interface JobData {
  payload: Payload;
  repoPath: string;
  /** The PR's source branch — the worktree's starting point and where
   * approve pushes back to. */
  fromBranch: string;
  pr: PullRequestInfo;
  /** The open root comments Claude was asked to address (plan.js's
   * collectOpenComments), normalized — the extension renders these, and
   * approve only posts replies to ids in this list. */
  comments: ReviewComment[];
  worktree?: WorktreeHandle;
  /** HEAD right after the worktree was created — the diff base, and how
   * approve tells "an earlier approve already committed" (HEAD moved). */
  preSha?: string;
  claudeSession?: ClaudeSession;
  /** Claude's parsed report, or null when it didn't return a readable one. */
  report?: ReportEntry[] | null;
  /** The default reply text per report entry (plan.js's defaultReplies) —
   * the extension shows it and sends back what the user keeps. */
  replies?: Array<{ commentId: number; action: ReportAction; text: string }>;
  /** Tree fingerprint of what the panel last showed — see
   * core/reviewed-push.ts's assertTreeMatchesReviewed. */
  reviewedTree?: string;
  /** The commit an approve made, set right after `git commit` succeeds.
   * If that approve then stopped before pushing (push failed, or a commit
   * hook changed files — see plan.js's commitTreeError), a later approve
   * accepts HEAD being this commit on top of preSha (plan.js's
   * approveGitSteps) instead of refusing a moved HEAD. */
  approveCommit?: string;
  /** Comment ids approve has already replied to, written after each reply
   * is posted, so a retried approve (after a restart cut the reply loop
   * short) skips them instead of posting them again. */
  repliesPosted?: number[];
  /** Same meaning as resolve-conflict's JobData.claudePid: set while the
   * headless claude runs, cleared when runClaude settles, and checked by
   * refreshDiff/approve after a restart (assertNoLiveClaude). */
  claudePid?: number;
  /** Same meaning as resolve-conflict's JobData.gitMetadataCompromised:
   * once set, no `git` may run in or against this worktree again. */
  gitMetadataCompromised?: boolean;
  /** Same meaning as resolve-conflict's JobData.gitMetadataUntrusted: why
   * the flag above was set, for the message only (untrustedMessage). */
  gitMetadataUntrusted?: "missing" | "changed";
}

const COMPROMISED_MESSAGE =
  "Claude modified this worktree's git metadata, so it was not trusted. Discard the job.";

const MISSING_BASELINE_MESSAGE =
  "This job's worktree-integrity record is missing (it may have been lost in a service restart), so its worktree isn't trusted. Discard the job and start again.";

const CLAUDE_STILL_RUNNING_MESSAGE =
  "Claude from before the service restarted may still be running in this worktree. Wait a minute and click Refresh diff again, or Discard the job.";

/** Same as resolve-conflict's assertNoLiveClaude: runs before the
 * integrity checks and any git in refreshDiff/approve, refusing (never
 * killing — the pid may have been reused) while a claude from before a
 * restart may still be writing this worktree. */
function assertNoLiveClaude(jobId: string, data: { claudePid?: number }): void {
  if (data.claudePid === undefined) return;
  if (isProcessGroupAlive(data.claudePid)) throw new JobStatusError(CLAUDE_STILL_RUNNING_MESSAGE);
  jobStore.patchData(jobId, { claudePid: undefined });
}

/** Running jobs by id — see resolve-conflict's copy of this for why. */
const running = new Map<string, { controller: AbortController; done: Promise<void> }>();

/* The git-metadata baseline for each job lives in the shared, persisted
 * store (core/integrity-baselines.js), not in job.data — nothing outside
 * the two worktree features (e.g. GET /status) has any use for it. runJob
 * takes it as soon as the worktree exists, before Claude runs, so every
 * job that has a worktree has a baseline, and a missing one is treated
 * as untrusted rather than as "Claude never touched it". */

class JobCancelledError extends Error {}

/** True only if the worktree's git metadata still matches the baseline.
 * A missing baseline, or an unreadable file, counts as not intact — this
 * check exists to notice the unexpected. */
function gitMetadataIntact(jobId: string, worktreeDir: string): boolean {
  return baselines.check(jobId, worktreeDir) === "intact";
}

/** Called before every git command the service runs in this worktree after
 * Claude has run (render, approve's stage/commit/push, cleanup): the
 * metadata that decides how git behaves here — hooks path, fsmonitor, the
 * gitlink — must still be what it was when the worktree was created. A
 * mismatch or a missing baseline marks the job compromised, with the
 * reason, moves it to "failed" (so the panel offers only Discard, never
 * Approve) and throws before any git runs. */
function assertGitMetadataIntact(jobId: string, worktreeDir: string): void {
  const state = baselines.check(jobId, worktreeDir);
  if (state === "intact") return;
  jobStore.patchData(jobId, { gitMetadataCompromised: true, gitMetadataUntrusted: state });
  const msg = untrustedMessage({ gitMetadataUntrusted: state });
  jobStore.update(jobId, { status: "failed", error: msg });
  throw new JobStatusError(msg);
}

/** Same as resolve-conflict's untrustedMessage: a missing baseline isn't
 * tampering, so it gets its own message; no reason recorded reads as
 * "changed". */
function untrustedMessage(data: { gitMetadataUntrusted?: "missing" | "changed" }): string {
  return data.gitMetadataUntrusted === "missing" ? MISSING_BASELINE_MESSAGE : COMPROMISED_MESSAGE;
}

/** "missing" only when there's no baseline at all; anything else not
 * intact is "changed". */
function untrustedReason(jobId: string, worktreeDir: string): "missing" | "changed" {
  return baselines.check(jobId, worktreeDir) === "missing" ? "missing" : "changed";
}

/** Remove the job's worktree, if it got that far. A compromised worktree
 * (flagged, or failing the check right now, including a missing
 * baseline) is removed with plain fs only
 * — see resolve-conflict's discardWorktree for why not even
 * `git worktree remove` may run then. */
async function discardWorktree(job: Job): Promise<void> {
  const data = job.data as unknown as Partial<JobData> | undefined;
  if (!data?.worktree) return;
  if (data.gitMetadataCompromised || !gitMetadataIntact(job.id, data.worktree.dir)) {
    // An already-flagged job keeps the reason it was flagged with.
    jobStore.patchData(job.id, {
      gitMetadataCompromised: true,
      gitMetadataUntrusted:
        data.gitMetadataUntrusted ??
        (data.gitMetadataCompromised ? "changed" : untrustedReason(job.id, data.worktree.dir)),
    });
    fs.rmSync(data.worktree.dir, { recursive: true, force: true });
    return;
  }
  await removeWorktree(data.worktree);
}

type RenderOutcome = { tree: string; fileDiffs: Array<{ path: string; diff: string }> };

/** Stage everything, fingerprint the tree, and diff it against preSha —
 * shared by runJob's diff step and refreshDiff. */
async function renderReview(jobId: string, worktreeDir: string, preSha: string): Promise<RenderOutcome> {
  assertGitMetadataIntact(jobId, worktreeDir);
  await stageAll(worktreeDir);
  const tree = await writeTree(worktreeDir);
  const files = (await git(["diff", "--name-only", "-z", "--no-renames", preSha, tree], worktreeDir)).stdout
    .split("\0")
    .filter(Boolean);
  return { tree, fileDiffs: await computeFileDiffs(worktreeDir, preSha, files) };
}

function reportCounts(report: ReportEntry[]): string {
  const n = (action: ReportAction) => report.filter((e) => e.action === action).length;
  return `${n("fixed")} fixed, ${n("declined")} declined, ${n("needs-discussion")} need discussion`;
}

function reviewSummary(data: JobData, fileCount: number, refreshed: boolean): string {
  const files = fileCount === 0 ? "no files changed" : fileCount === 1 ? "1 file changed" : `${fileCount} files changed`;
  const agent = (data as { agent?: string }).agent === "cursor" ? "Cursor Agent" : "Claude";
  const report = data.report
    ? `${agent} went through ${data.comments.length} open comment(s): ${reportCounts(data.report)}; ${files}.`
    : `${agent} went through ${data.comments.length} open comment(s) but didn't return a readable report, ` +
      `so no replies were prepared; ${files}.`;
  return `${refreshed ? "Refreshed diff. " : ""}${report} Review the diff and replies below before approving.`;
}

export function createAddressReviewCommentsFeature(config: Config): Feature {
  return {
    id: "address-review-comments",
    label: "Address review comments",

    async start(payloadRaw: unknown, ctx): Promise<Job> {
      const payload = plan.validatePayload(payloadRaw);
      // Validated before a job exists, so a bad config.json entry is an
      // immediate, actionable error rather than a job that fails later.
      // Trimmed once here: the prompt, the --allowedTools rules and the
      // bash guard (derived from those rules) all see the same list.
      const checkCommands = claudeArgs.normalizeCheckCommands(config.addressReviewComments?.checkCommands);
      const checkRules = claudeArgs.checkCommandRules(checkCommands);
      const repo = resolveRepoPath(config, payload.project, payload.repo);
      const job = jobStore.create("address-review-comments", { payload });
      const controller = new AbortController();

      const done = runJob(job.id, repo, payload, config, ctx.auth, checkCommands, checkRules, controller.signal)
        .catch(async (err: Error) => {
          if (!controller.signal.aborted) {
            jobStore.update(job.id, { status: "failed", error: err.message });
            return;
          }
          await discardWorktree(job).catch((cleanupErr: Error) =>
            console.warn(`[address-review-comments] cleanup after cancel failed: ${cleanupErr.message}`),
          );
          baselines.remove(job.id);
          jobStore.update(job.id, { status: "rejected", progress: undefined });
        })
        .finally(() => running.delete(job.id));
      running.set(job.id, { controller, done });
      return job;
    },

    async cancel(job: Job): Promise<void> {
      // Locked for the same reason as resolve-conflict's cancel().
      await withJobLock(job.id, async () => {
        const current = jobStore.get(job.id);
        const entry = running.get(job.id);
        if (!entry || !current || current.status !== "running") {
          throw new Error(`Cannot cancel job in status "${current?.status ?? job.status}"`);
        }
        jobStore.update(job.id, {
          status: "rejecting",
          progress: { stepId: "cancel", label: "Cancelling and discarding the changes…" },
        });
        entry.controller.abort(new JobCancelledError("cancelled"));
        await entry.done;
      });
    },

    async approve(job: Job, ctx): Promise<void> {
      await withJobLock(job.id, async () => {
        const current = jobStore.get(job.id);
        if (!current || current.status !== "awaiting-approval") {
          throw new Error(`Cannot approve job in status "${current?.status ?? "unknown"}"`);
        }
        const data = current.data as unknown as JobData;
        assertNoLiveClaude(job.id, data);
        if (data.gitMetadataCompromised) throw new JobStatusError(untrustedMessage(data));
        if (!data.worktree || !data.preSha) {
          throw new JobStatusError("This job has no worktree to push.");
        }
        const dir = data.worktree.dir;
        // Validated against this job's own comments before anything runs,
        // so what gets posted is decided up front, not after the push.
        const replies = plan.selectReplies((ctx.body as { replies?: unknown } | undefined)?.replies, data.comments);
        jobStore.update(job.id, { status: "approving" });

        let pushed = false;
        try {
          assertGitMetadataIntact(job.id, dir);
          await stageAll(dir);
          const tree = await writeTree(dir);
          assertTreeMatchesReviewed(tree, data.reviewedTree);

          const stagedClean = (await git(["diff", "--cached", "--quiet"], dir, { allowFailure: true })).code === 0;
          const head = (await git(["rev-parse", "HEAD"], dir)).stdout.trim();
          const parent = await git(["rev-parse", "-q", "--verify", "HEAD^"], dir, { allowFailure: true });
          const steps = plan.approveGitSteps({
            stagedClean,
            head,
            headParent: parent.code === 0 ? parent.stdout.trim() : null,
            preSha: data.preSha,
            approveCommit: data.approveCommit,
          });
          if (steps.error) throw new Error(steps.error);

          if (steps.resetTo) {
            // Our own unpushed commit plus newly reviewed changes: fold
            // them into one fresh commit on preSha (see approveGitSteps).
            assertGitMetadataIntact(job.id, dir);
            await git(["reset", "--soft", steps.resetTo], dir);
          }
          if (steps.commit) {
            assertGitMetadataIntact(job.id, dir);
            // Hooks ON (no --no-verify), unlike resolve-conflict's merge
            // commit: these are new code changes on the PR, exactly what
            // the repo's pre-commit checks exist for.
            try {
              await git(["commit", "-m", plan.commitMessage({ id: data.payload.prId }, data.report ?? null)], dir);
            } catch (err) {
              throw new Error(
                `git commit was refused — usually a pre-commit hook failing. Nothing was pushed and the ` +
                  `worktree is kept: fix it (e.g. Continue in Claude Code), click Refresh diff, then approve ` +
                  `again.\n\n${(err as Error).message}`,
              );
            }
            // Recorded before the tree check below, so a retry (after
            // Refresh diff) recognizes this commit as ours.
            const approveCommit = (await git(["rev-parse", "HEAD"], dir)).stdout.trim();
            jobStore.patchData(job.id, { approveCommit });
            data.approveCommit = approveCommit;
          }
          if (steps.push) {
            // Re-read the commit about to be pushed, on every path (fresh
            // commit or a retried push of our earlier one): it must be one
            // commit on preSha whose tree is exactly what Refresh diff last
            // showed — a hook may have rewritten and re-staged files during
            // the commit (plan.js's prePushError). Then push that exact
            // sha, not `HEAD`, so what was checked is what goes out.
            assertGitMetadataIntact(job.id, dir);
            const [pushSha, headParent, headTree] = (
              await git(["rev-parse", "HEAD", "HEAD^", "HEAD^{tree}"], dir)
            ).stdout
              .trim()
              .split("\n")
              .map((line) => line.trim());
            const pushError = plan.prePushError({
              headParent: headParent || null,
              headTree,
              preSha: data.preSha,
              reviewedTree: data.reviewedTree,
            });
            if (pushError) throw new Error(pushError);
            assertGitMetadataIntact(job.id, dir);
            await git(["push", "origin", `${pushSha}:refs/heads/${data.fromBranch}`], dir);
            pushed = true;
          }
        } catch (err) {
          // Worktree left in place (see resolve-conflict's approve) so the
          // user can inspect or fix it; Refresh diff brings the job back.
          jobStore.update(job.id, { status: "failed", error: (err as Error).message });
          throw err;
        }

        // After the push: a reply failing no longer un-approves anything —
        // the code is on the branch. Name the failures in the summary.
        // Each reply is recorded the moment it's posted, and ones an
        // earlier approve already posted are skipped: a restart mid-loop
        // (the job comes back failed; Refresh diff, then Approve again)
        // would otherwise post every reply a second time.
        const { git: host } = providersFor(config);
        const alreadyPosted = new Set(data.repliesPosted ?? []);
        const posted: number[] = [...alreadyPosted];
        const failed: Array<{ commentId: number; error: string }> = [];
        for (const reply of replies) {
          if (alreadyPosted.has(reply.commentId)) continue;
          try {
            await host.replyToComment(
              ctx.auth,
              data.payload.project,
              data.payload.repo,
              data.payload.prId,
              reply.commentId,
              reply.text,
            );
            posted.push(reply.commentId);
            jobStore.patchData(job.id, { repliesPosted: [...posted] });
          } catch (err) {
            failed.push({ commentId: reply.commentId, error: (err as Error).message });
          }
        }

        await discardWorktree(current).catch((err: Error) =>
          console.warn(`[address-review-comments] removing the worktree after approve failed: ${err.message}`),
        );
        baselines.remove(job.id);
        jobStore.patchData(job.id, { repliesPosted: posted, repliesFailed: failed });
        jobStore.update(job.id, {
          status: "approved",
          result: {
            ...(current.result ?? { files: [] }),
            summary: plan.approveSummary({ pushed, branch: data.fromBranch, posted, failed }),
          },
        });
      });
    },

    async reject(job: Job): Promise<void> {
      // Same shape as resolve-conflict's reject(): an advisory fast-path,
      // then the lock and a re-read.
      if (job.status === "approving" || job.status === "rejecting") {
        throw new JobStatusError(`This job is already ${job.status}.`);
      }
      await withJobLock(job.id, async () => {
        const current = jobStore.get(job.id);
        if (!current) throw new JobStatusError(`Unknown job id: ${job.id}`);
        if (!canReject(current.status)) throw new JobStatusError(`This job is already ${current.status}.`);
        jobStore.update(job.id, { status: "rejecting" });
        await discardWorktree(current);
        baselines.remove(job.id);
        jobStore.update(job.id, { status: "rejected" });
      });
    },

    async refreshDiff(job: Job): Promise<Job> {
      return withJobLock(job.id, async () => {
        const current = jobStore.get(job.id);
        if (!current) throw new JobStatusError(`Unknown job id: ${job.id}`);
        if (current.status !== "awaiting-approval" && current.status !== "failed") {
          throw new JobStatusError(`Cannot refresh the diff for a job in status "${current.status}"`);
        }
        const data = current.data as unknown as JobData;
        assertNoLiveClaude(job.id, data);
        if (data.gitMetadataCompromised) throw new JobStatusError(untrustedMessage(data));
        if (!data.worktree || !data.preSha) {
          throw new JobStatusError("There's nothing to refresh — this job failed before Claude changed anything.");
        }
        if (!fs.existsSync(data.worktree.dir)) {
          throw new WorktreeGoneError(
            "The worktree for this job no longer exists (it may already have been approved or discarded).",
          );
        }
        const outcome = await renderReview(job.id, data.worktree.dir, data.preSha);
        jobStore.patchData(job.id, { reviewedTree: outcome.tree });
        return jobStore.update(job.id, {
          status: "awaiting-approval",
          error: undefined,
          result: { summary: reviewSummary(data, outcome.fileDiffs.length, true), files: outcome.fileDiffs },
        });
      });
    },

    // Only ever queues, same as resolve-conflict's start_resolve_conflict:
    // this feature's Claude run edits and its approve pushes and replies,
    // so nothing runs until the user clicks Start on the PR page.
    mcpTools() {
      return [
        {
          name: "start_address_review_comments",
          async handler(args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
            const project = args.project as string;
            const repo = args.repo as string;
            const prId = args.prId as number;
            await fetchOpenPullRequest(config, ctx.auth, project, repo, prId);
            const job = ctx.createPendingJob("address-review-comments", { project, repo, prId });
            return pendingStartResult(config, job, project, repo, prId);
          },
        },
      ];
    },
  };
}

async function runJob(
  jobId: string,
  repoPath: string,
  payload: Payload,
  config: Config,
  auth: AuthContext,
  checkCommands: string[],
  checkRules: string[],
  signal: AbortSignal,
): Promise<void> {
  const checkpoint = () => {
    if (signal.aborted) throw new JobCancelledError("cancelled");
  };
  const { git: host } = providersFor(config);

  jobStore.update(jobId, { progress: { stepId: "fetch", label: "Fetching the pull request from Bitbucket…" } });
  const pr = (await host.getPullRequest(auth, payload.project, payload.repo, payload.prId)) as PullRequestInfo | null;
  if (!pr || !pr.fromBranch) {
    throw new Error(
      `Bitbucket didn't return a usable pull request for ${payload.project}/${payload.repo} #${payload.prId}.`,
    );
  }
  if (pr.state && pr.state !== "OPEN") {
    throw new Error(`Pull request #${payload.prId} is ${pr.state.toLowerCase()} — there's nothing to push to.`);
  }
  // Before any worktree exists: the worktree comes from, and approve
  // pushes to, this clone's `origin` — only right when the PR's source and
  // target are both the payload's repo (not a fork).
  const repoError = plan.pullRequestRepoError(pr, payload);
  if (repoError) throw new Error(repoError);
  // Only used to skip my own comments nobody has answered; unknown just
  // means that filter is skipped (plan.js's collectOpenComments).
  const me = await host.whoAmI(auth).catch((err: Error) => {
    console.warn(`[address-review-comments] couldn't tell who's logged in to Bitbucket: ${err.message}`);
    return null;
  });
  jobStore.patchData(jobId, { repoPath, fromBranch: pr.fromBranch, pr });

  checkpoint();
  jobStore.update(jobId, { progress: { stepId: "comments", label: "Collecting open review comments…" } });
  const all = (await host.listActivities(auth, payload.project, payload.repo, payload.prId)) as unknown as ReviewComment[];
  const comments = plan.collectOpenComments(all, me);
  jobStore.patchData(jobId, { comments });
  if (comments.length === 0) {
    jobStore.update(jobId, {
      status: "failed",
      error:
        "No open review comments to address — resolved threads, your own unanswered comments, and comments " +
        "on removed or outdated code are skipped.",
    });
    return;
  }

  // The default editor picks the agent: Cursor Agent when it's Cursor, Claude
  // otherwise (VS Code's CLI can't run headlessly, so it also uses Claude).
  const editor = resolveReviewEditor(config.reviewEditor);
  const useCursor = editor === "cursor";
  const agentName = useCursor ? "Cursor Agent" : "Claude";
  const model = config.addressReviewComments?.model || undefined;
  jobStore.patchData(jobId, { editor: editor || "claude-code", agent: useCursor ? "cursor" : "claude" });

  checkpoint();
  jobStore.update(jobId, {
    progress: {
      stepId: "address",
      label:
        comments.length === 1
          ? `Addressing 1 review comment with ${agentName} — this can take a few minutes…`
          : `Addressing ${comments.length} review comments with ${agentName} — this can take a few minutes…`,
    },
  });
  const worktree = await addWorktree(repoPath, pr.fromBranch, jobId);
  // Recorded before anything else can throw, so cancel/reject can find it.
  jobStore.patchData(jobId, { worktree });
  checkpoint();
  // Resolved and snapshotted as soon as the worktree exists — see
  // resolve-conflict's runJob. From here on every git the service runs in
  // this worktree is preceded by a check against this baseline.
  const gitDir = (await git(["rev-parse", "--absolute-git-dir"], worktree.dir)).stdout.trim();
  baselines.take(jobId, { worktreeDir: worktree.dir, gitDir });
  const preSha = (await git(["rev-parse", "HEAD"], worktree.dir)).stdout.trim();
  // `origin/<fromBranch>` must be the very commit Bitbucket just reported
  // as the PR's source — else the branch moved, or this clone's `origin`
  // has a same-named branch that isn't the PR's (plan.js's sourceShaError).
  // Checked before preSha is recorded, so a mismatch leaves nothing
  // approve could push.
  const shaError = plan.sourceShaError(preSha, pr.fromSha);
  if (shaError) throw new Error(shaError);
  jobStore.patchData(jobId, { preSha });

  // Confinement identical to resolve-conflict's runJob (see its comments
  // for the reasoning behind each piece): sandboxed Bash that may also
  // write the worktree's git dir, minus the four metadata files that could
  // redirect the service's own next git command; the edit guard pinned to
  // the worktree's real path; and the metadata baseline taken right after
  // addWorktree, compared against before any later git.
  const sandboxDenyWrite = [
    path.join(worktree.dir, ".git"),
    path.join(gitDir, "commondir"),
    path.join(gitDir, "gitdir"),
    path.join(gitDir, "config.worktree"),
  ];
  const editRoot = fs.realpathSync(worktree.dir);
  const sessionId = randomUUID();
  checkpoint();

  // claudePid is persisted with the job while the agent runs, and cleared
  // however the run ends — see JobData.claudePid (it holds Cursor Agent's
  // pid too).
  const prompt = plan.buildAddressCommentsPrompt(comments, pr, { checkCommands });
  const onSpawn = (pid: number) => jobStore.patchData(jobId, { claudePid: pid });
  let result: { text: string; sessionId?: string };
  try {
    result = useCursor
      ? await runCursorAgent({ cwd: worktree.dir, prompt, model, signal, onSpawn })
      : await runClaude({
          cwd: worktree.dir,
          prompt,
          model,
          policy: "worktreeWriteNarrowBash",
          extraAllowedTools: checkRules,
          sessionId,
          sandboxAllowWrite: [gitDir],
          sandboxDenyWrite,
          editRoot,
          signal,
          onSpawn,
        });
  } finally {
    jobStore.patchData(jobId, { claudePid: undefined });
  }

  if (!gitMetadataIntact(jobId, worktree.dir)) {
    const gitMetadataUntrusted = untrustedReason(jobId, worktree.dir);
    jobStore.patchData(jobId, { gitMetadataCompromised: true, gitMetadataUntrusted });
    jobStore.update(jobId, { status: "failed", error: untrustedMessage({ gitMetadataUntrusted }) });
    return;
  }

  // Only a Claude run leaves a session "Continue in Claude Code" can resume.
  const claudeSession: ClaudeSession | undefined = useCursor
    ? undefined
    : { id: result.sessionId || sessionId, cwd: worktree.dir, permissionMode: "default" };
  const report = plan.parseAddressReport(
    result.text,
    comments.map((c) => c.id),
  );
  jobStore.patchData(jobId, { claudeSession, report, replies: plan.defaultReplies(report) });

  checkpoint();
  jobStore.update(jobId, { progress: { stepId: "diff", label: "Preparing diff for review…" } });
  const outcome = await renderReview(jobId, worktree.dir, preSha);
  checkpoint();
  jobStore.patchData(jobId, { reviewedTree: outcome.tree });
  const data = jobStore.get(jobId)!.data as unknown as JobData;
  jobStore.update(jobId, {
    status: "awaiting-approval",
    result: { summary: reviewSummary(data, outcome.fileDiffs.length, false), files: outcome.fileDiffs },
  });
}
