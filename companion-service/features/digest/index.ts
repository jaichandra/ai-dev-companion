// Morning digest (#8): your PRs (conflicts, builds, approvals, needs-work),
// PRs waiting on your review, your open tickets and what the watchers
// pre-warmed — one read-only view. A ✨ row on every covered page runs it
// on a click; the get_digest MCP tool and `companion digest` compute it
// directly; the scheduler posts it to the inbox on weekdays at digest.time.
// Every source is best effort: one that fails becomes a "Couldn't check"
// line, never a failed digest.
import type { Config } from "../../config";
import type { AuthContext } from "../../core/atlassian";
import { AuthSetupError, HttpStatusError } from "../../core/atlassian";
import { BuildSummary, DashboardPullRequest, IssueSummary, providersFor } from "../../core/providers";
import { Feature, FeatureContext, Job, JobStatusError, McpToolContext, jobStore } from "../../core/jobs";
import type { TaskFn } from "../../core/scheduler";

export interface Digest {
  generatedAt: string;
  headline: string;
  counts: Record<string, number>;
  sections: { id: string; title: string; items: { text: string; tone: string; url: string | null }[] }[];
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  buildDigest(input: {
    now: number;
    myPrs?: { pr: DashboardPullRequest; conflicted?: boolean | null; build?: BuildSummary | null }[];
    reviewPrs?: DashboardPullRequest[];
    tickets?: IssueSummary[];
    prewarmed?: Record<string, unknown>[];
    notes?: string[];
  }): Digest;
  digestNotification(digest: Digest, dayKey: string): Record<string, unknown>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const schedule = require("../../core/schedule.js") as { localDayKey(now: number): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { redactSecrets } = require("../../core/history-record.js") as { redactSecrets(text: string): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const linkGuard = require("../../core/link-guard.js") as { linkOnHosts(url: unknown, baseUrls: string[]): string | null };

const MAX_MY_PRS = 10;
const PREWARMED_MAX_AGE_MS = 3 * 24 * 60 * 60 * 1000;
const OPEN_TICKETS_JQL = "assignee = currentUser() AND statusCategory != Done AND issuetype not in subTaskIssueTypes() ORDER BY priority DESC, updated DESC";

/** Everything the digest reads. */
export interface DigestIo {
  listDashboardPullRequests(auth: AuthContext, role: "AUTHOR" | "REVIEWER"): Promise<DashboardPullRequest[]>;
  getMergeStatus(auth: AuthContext, project: string, repo: string, prId: number): Promise<{ conflicted: boolean | null }>;
  getCommitBuildStatus(auth: AuthContext, project: string, repo: string, sha: string): Promise<BuildSummary | null>;
  searchIssues(auth: AuthContext, jql: string): Promise<IssueSummary[]>;
  jobs(): Job[];
}

export function defaultDigestIo(config: Config): DigestIo {
  const { git, issues } = providersFor(config);
  return {
    listDashboardPullRequests: (auth, role) => git.listDashboardPullRequests(auth, role),
    getMergeStatus: (auth, p, r, id) => git.getMergeStatus(auth, p, r, id),
    getCommitBuildStatus: (auth, p, r, sha) => git.getCommitBuildStatus(auth, p, r, sha),
    searchIssues: (auth, jql) => issues.searchIssues(auth, jql, 20),
    jobs: () => jobStore.list(),
  };
}

function why(err: unknown): string {
  if (err instanceof AuthSetupError) return "no sign-in (open the site in Chrome, or save a token in ⚙ Settings)";
  if (err instanceof HttpStatusError) return `HTTP ${err.status}`;
  return redactSecrets((err as Error)?.message || String(err)).slice(0, 160);
}

/** Collects every source and builds the digest. Never throws for a source. */
export async function computeDigest(config: Config, io: DigestIo, auth: AuthContext, now: number): Promise<Digest> {
  // The digest's links open on a click: https on the configured Bitbucket or Jira host only.
  const { git, issues } = providersFor(config);
  const hosts = [git.baseUrl(), issues.baseUrl()];
  const safe = <T extends { url: string | null }>(item: T): T => ({ ...item, url: linkGuard.linkOnHosts(item.url, hosts) });
  const notes: string[] = [];
  const myPrs: { pr: DashboardPullRequest; conflicted: boolean | null; build: BuildSummary | null }[] = [];
  try {
    const mine = (await io.listDashboardPullRequests(auth, "AUTHOR")).slice(0, MAX_MY_PRS);
    let mergeFailed = 0;
    let buildFailed = 0;
    for (const pr of mine) {
      let conflicted: boolean | null = null;
      let build: BuildSummary | null = null;
      try {
        conflicted = (await io.getMergeStatus(auth, pr.project, pr.repo, pr.id)).conflicted;
      } catch {
        mergeFailed++;
      }
      try {
        build = pr.fromSha ? await io.getCommitBuildStatus(auth, pr.project, pr.repo, pr.fromSha) : null;
      } catch {
        buildFailed++;
      }
      myPrs.push({ pr: safe(pr), conflicted, build });
    }
    if (mergeFailed) notes.push(`Couldn't check ${mergeFailed} of your PRs for conflicts.`);
    if (buildFailed) notes.push(`Couldn't read the builds of ${buildFailed} of your PRs.`);
  } catch (err) {
    notes.push(`Couldn't read your pull requests: ${why(err)}.`);
  }
  let reviewPrs: DashboardPullRequest[] = [];
  try {
    reviewPrs = (await io.listDashboardPullRequests(auth, "REVIEWER")).map(safe);
  } catch (err) {
    notes.push(`Couldn't read your review queue: ${why(err)}.`);
  }
  let tickets: IssueSummary[] = [];
  try {
    tickets = (await io.searchIssues(auth, OPEN_TICKETS_JQL)).map(safe);
  } catch (err) {
    notes.push(`Couldn't read your Jira tickets: ${why(err)}.`);
  }
  const prewarmed = io
    .jobs()
    .filter((j) => j.startedVia === "watcher" && (j.status === "awaiting-approval" || j.status === "failed") && now - j.createdAt < PREWARMED_MAX_AGE_MS)
    .map((j) => ({
      featureId: j.featureId,
      status: j.status,
      scopeKey: j.scopeKey,
      summary: j.result?.summary,
      error: typeof j.error === "string" ? redactSecrets(j.error) : j.error,
    }));
  return plan.buildDigest({ now, myPrs, reviewPrs, tickets, prewarmed, notes });
}

/** The scheduler's weekday digest: posted to the inbox (quiet-hours items ride along). */
export function createDigestTask(opts: {
  config: Config;
  notifications: { add(input: Record<string, unknown>): unknown; absorbPending(): string[] };
  io?: DigestIo;
}): TaskFn {
  return async (ctx) => {
    const now = ctx.now();
    const digest = await computeDigest(opts.config, opts.io || defaultDigestIo(opts.config), {}, now);
    const absorbed = opts.notifications.absorbPending().length;
    opts.notifications.add(plan.digestNotification(digest, schedule.localDayKey(now)));
    return { ...digest.counts, absorbed, notes: digest.sections.find((s) => s.id === "notes")?.items.length || 0 };
  };
}

export function createDigestFeature(config: Config, _deps: unknown = {}, io: DigestIo = defaultDigestIo(config)): Feature {
  return {
    id: "digest",
    label: "Morning digest",

    // Polled read-only shape (like the ticket workspace).
    async start(_payload: unknown, ctx: FeatureContext): Promise<Job> {
      const job = jobStore.create("digest", {});
      jobStore.setProgressIfRunning(job.id, "collect", "Reading your PRs, reviews and tickets…");
      void computeDigest(config, io, ctx.auth, Date.now())
        .then((digest) =>
          jobStore.update(job.id, {
            status: "awaiting-approval",
            progress: undefined,
            data: { digest },
            result: { summary: digest.headline, files: [] },
          }),
        )
        .catch((err: Error) => jobStore.update(job.id, { status: "failed", progress: undefined, error: redactSecrets(err?.message || String(err)) }));
      return job;
    },

    async approve(job: Job): Promise<void> {
      throw new JobStatusError(`Cannot approve job in status "${job.status}" — the digest is a read-only report`);
    },

    async reject(job: Job): Promise<void> {
      if (job.status === "awaiting-approval" || job.status === "failed") {
        jobStore.update(job.id, { status: "rejected", progress: undefined });
        return;
      }
      throw new JobStatusError(`Cannot reject job in status "${job.status}"`);
    },

    mcpTools() {
      return [
        {
          name: "get_digest",
          async handler(_args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
            return computeDigest(config, io, ctx.auth, Date.now());
          },
        },
      ];
    },
  };
}
