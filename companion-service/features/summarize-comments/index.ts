// Summarize comments: a short summary of a Jira ticket's comment thread,
// written by a small fast model (Haiku by default). Read-only — it reads the
// comments through the Jira client and runs `claude -p` with no tools in a
// scratch directory, so the model can neither touch files nor reach the
// network. The extension only offers it on tickets that have comments.
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import type { Config } from "../../config";
import { DEFAULT_SUMMARIZE_COMMENTS_MODEL } from "../../config";
import { runClaude } from "../../core/claude";
import { Feature, FeatureContext, Job, JobStatusError, jobStore } from "../../core/jobs";
import { providersFor } from "../../core/providers";

interface Comment {
  author: string;
  created: string | null;
  body: string;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  normalizeComments(raw: unknown): Comment[];
  selectWithinBudget(comments: Comment[], budget?: number): { kept: Comment[]; omitted: number };
  buildPrompt(input: { issueKey: string; summary: string | null; kept: Comment[]; omitted: number }): string;
  parseSummary(text: string): Record<string, unknown>;
  readSummaryCache(issueKey: string): { commentCount: number; omitted: number; summary: Record<string, unknown>; completedAt: string } | null;
  writeSummaryCache(entry: { issueKey: string; commentCount: number; omitted: number; summary: Record<string, unknown>; completedAt: string }): void;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { redactSecrets } = require("../../core/history-record.js") as { redactSecrets(text: string): string };

const ISSUE_KEY_RE = /^[A-Z][A-Z0-9_]*-\d+$/;

function parseIssueKey(raw: unknown): string {
  const key = String(raw ?? "").trim().toUpperCase();
  if (!ISSUE_KEY_RE.test(key)) throw new Error(`"${raw}" doesn't look like a Jira issue key (expected e.g. "PROJ-34034").`);
  return key;
}

export function createSummarizeCommentsFeature(config: Config): Feature {
  const running = new Map<string, { controller: AbortController; done: Promise<void> }>();

  async function run(jobId: string, issueKey: string, force: boolean, ctx: FeatureContext, signal: AbortSignal): Promise<void> {
    const { issues } = providersFor(config);
    jobStore.update(jobId, { progress: { stepId: "fetch", label: "Reading comments…" } });
    const [basics, rawComments] = await Promise.all([
      issues.getIssueBasics(ctx.auth, issueKey).catch(() => null),
      issues.getIssueComments(ctx.auth, issueKey),
    ]);
    const comments = plan.normalizeComments(rawComments);
    if (comments.length === 0) throw new Error(`${issueKey} has no comments to summarize.`);
    // A saved summary is reused while the thread has the same number of comments.
    const saved = force ? null : plan.readSummaryCache(issueKey);
    if (saved && saved.commentCount === comments.length) {
      jobStore.update(jobId, {
        status: "awaiting-approval",
        progress: undefined,
        data: { issueKey, title: basics?.summary ?? null, summary: saved.summary, commentCount: saved.commentCount, omitted: saved.omitted, fromCache: true, completedAt: saved.completedAt },
        result: { summary: `Summarized ${saved.commentCount} comments on ${issueKey}`, files: [] },
      });
      return;
    }
    const { kept, omitted } = plan.selectWithinBudget(comments);

    jobStore.update(jobId, { progress: { stepId: "summarize", label: `Summarizing ${comments.length} comments…` } });
    // A scratch directory only: the run has no tools, so nothing is read or written there.
    const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "summarize-comments-"));
    try {
      const result = await runClaude({
        cwd,
        prompt: plan.buildPrompt({ issueKey, summary: basics?.summary ?? null, kept, omitted }),
        model: config.summarizeComments?.model || DEFAULT_SUMMARIZE_COMMENTS_MODEL,
        policy: "noTools",
        signal,
      });
      if (signal.aborted) return;
      const summary = plan.parseSummary(redactSecrets(result.text));
      const completedAt = new Date().toISOString();
      try {
        plan.writeSummaryCache({ issueKey, commentCount: comments.length, omitted, summary, completedAt });
      } catch (err) {
        console.warn(`[summarize-comments] could not save the summary: ${err instanceof Error ? err.message : err}`);
      }
      jobStore.update(jobId, {
        status: "awaiting-approval",
        progress: undefined,
        data: { issueKey, title: basics?.summary ?? null, summary, commentCount: comments.length, omitted, completedAt },
        result: { summary: `Summarized ${comments.length} comments on ${issueKey}`, files: [] },
      });
    } finally {
      fs.rmSync(cwd, { recursive: true, force: true });
    }
  }

  return {
    id: "summarize-comments",
    label: "Summarize comments",

    // Polled read-only shape (like analyze-issue): /start returns the running
    // job; it lands in awaiting-approval with data.summary.
    async start(payload: unknown, ctx: FeatureContext): Promise<Job> {
      const p = payload as { issueKey?: unknown; force?: unknown } | null;
      const issueKey = parseIssueKey(p?.issueKey);
      const job = jobStore.create("summarize-comments", { issueKey });
      const controller = new AbortController();
      const done = run(job.id, issueKey, p?.force === true, ctx, controller.signal)
        .catch((err: Error) => {
          if (controller.signal.aborted) jobStore.update(job.id, { status: "rejected", progress: undefined });
          else jobStore.update(job.id, { status: "failed", progress: undefined, error: redactSecrets(err?.message || String(err)) });
        })
        .finally(() => running.delete(job.id));
      running.set(job.id, { controller, done });
      return job;
    },

    async cancel(job: Job): Promise<void> {
      const entry = running.get(job.id);
      if (!entry || job.status !== "running") throw new Error(`Cannot cancel job in status "${job.status}"`);
      jobStore.update(job.id, { status: "rejecting", progress: { stepId: "cancel", label: "Cancelling…" } });
      entry.controller.abort();
      await entry.done;
    },

    async approve(job: Job): Promise<void> {
      throw new JobStatusError(`Cannot approve job in status "${job.status}" — summarize-comments is a read-only report`);
    },

    async reject(job: Job): Promise<void> {
      if (job.status === "awaiting-approval" || job.status === "failed") {
        jobStore.update(job.id, { status: "rejected", progress: undefined });
        return;
      }
      throw new JobStatusError(`Cannot reject job in status "${job.status}"`);
    },
  };
}
