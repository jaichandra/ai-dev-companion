// After Start fix the fix is committed, pushed and opened as a pull request
// from the Claude Code terminal, where Bitbucket fills in the default
// reviewers and Jira links the PR by the ticket key in its branch and commits.
// The one step left is moving the ticket to its review status. This does it,
// on its own: a poll looks for an open pull request from each tracked job's
// branch and moves the ticket when there is one (Settings can turn that off);
// the panel's "Move ticket to In Review" does it on request. The job then
// ends as "approved" with the PR recorded, which is what the history counts
// as Start fix to PR.
import type { Config } from "../../config";
import type { AuthContext } from "../../core/atlassian";
import { Job, jobStore } from "../../core/jobs";
import { JiraTransition, PullRequestSummary, providersFor } from "../../core/providers";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  DEFAULT_REVIEW_TRANSITION: string;
  runReviewMove(
    input: { issueKey: string; transitionName: string },
    deps: {
      listTransitions(): Promise<JiraTransition[]>;
      pickTransition(list: JiraTransition[], wanted: string): JiraTransition | null;
      transition(id: string): Promise<void>;
    },
  ): Promise<{ status: "moved" | "no-transition" | "failed"; detail: string }>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const workflow = require("../../core/jira-workflow.js") as {
  pickTransition(list: JiraTransition[], wanted: string): JiraTransition | null;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const workspacePlan = require("../ticket-workspace/plan.js") as {
  prUrlAllowed(url: unknown, bitbucketBaseUrl: string): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { isSafeBranchName } = require("../../core/bitbucket-endpoints.js") as { isSafeBranchName(name: unknown): boolean };

export const POLL_INTERVAL_MS = 3 * 60 * 1000;

/** What the mover reads and writes outside this process; tests inject it. */
export interface MoverIo {
  findOpenPr(auth: AuthContext, project: string, repo: string, branch: string): Promise<PullRequestSummary | null>;
  listTransitions(auth: AuthContext, issueKey: string): Promise<JiraTransition[]>;
  transition(auth: AuthContext, issueKey: string, id: string): Promise<void>;
}

export function realMoverIo(config: Config): MoverIo {
  const { git, issues } = providersFor(config);
  return {
    findOpenPr: async (auth, project, repo, branch) =>
      (await git.listBranchPullRequests(auth, project, repo, branch)).find((pr) => pr.state === "OPEN") || null,
    listTransitions: (auth, issueKey) => issues.listTransitions(auth, issueKey),
    transition: (auth, issueKey, id) => issues.transitionIssue(auth, issueKey, id),
  };
}

interface TrackedData {
  issueKey?: string;
  repoKey?: string;
  branch?: string;
  reviewMove?: { moved: boolean; status: string; detail: string; at: number };
}

export interface MoveOutcome {
  /** "waiting": no pull request yet (polling only). */
  status: "moved" | "no-transition" | "failed" | "waiting" | "already";
  detail: string;
  prId?: number;
}

export function createReviewMover(config: Config, io: MoverIo = realMoverIo(config)) {
  const inFlight = new Set<string>();
  let timer: NodeJS.Timeout | null = null;

  const dataOf = (job: Job): TrackedData => job.data as unknown as TrackedData;

  function trackable(job: Job): { issueKey: string; project: string; repo: string; branch: string } | null {
    if (job.featureId !== "ticket-to-pr" || job.status !== "awaiting-approval") return null;
    const d = dataOf(job);
    if (!d.issueKey || !d.repoKey || !d.branch || !isSafeBranchName(d.branch)) return null;
    const [project, repo, ...rest] = d.repoKey.split("/");
    if (!project || !repo || rest.length > 0) return null;
    return { issueKey: d.issueKey, project, repo, branch: d.branch };
  }

  /** A PR address is only kept when it's a PR page on the configured Bitbucket. */
  function safePr(pr: PullRequestSummary): { id: number; url: string | null; title: string | null } {
    const base = providersFor(config).git.baseUrl();
    const ok = typeof pr.url === "string" && workspacePlan.prUrlAllowed(pr.url, base);
    return { id: pr.id, url: ok ? (pr.url as string) : null, title: pr.title ?? null };
  }

  /**
   * One job: look for the open pull request (when `needPr`, nothing is done
   * without one), move the ticket unless that's already settled, and end the
   * job once there is a PR. Never throws: a failure is recorded on the job
   * and tried again on the next poll.
   */
  async function check(job: Job, auth: AuthContext, needPr: boolean): Promise<MoveOutcome> {
    const t = trackable(job);
    if (!t) return { status: "already", detail: "This job isn't waiting for a pull request." };
    if (inFlight.has(job.id)) return { status: "already", detail: "Already checking this job." };
    inFlight.add(job.id);
    try {
      let pr: PullRequestSummary | null = null;
      try {
        pr = await io.findOpenPr(auth, t.project, t.repo, t.branch);
      } catch (err) {
        if (needPr) return { status: "failed", detail: `Couldn't look for the pull request: ${(err as Error).message}` };
      }
      if (needPr && !pr) return { status: "waiting", detail: `No open pull request from ${t.branch} yet.` };

      let outcome: MoveOutcome;
      const previous = dataOf(job).reviewMove;
      if (previous && previous.moved) {
        outcome = { status: "already", detail: previous.detail };
      } else {
        const result = await plan.runReviewMove(
          { issueKey: t.issueKey, transitionName: config.ticketToPr?.reviewTransitionName || plan.DEFAULT_REVIEW_TRANSITION },
          {
            listTransitions: () => io.listTransitions(auth, t.issueKey),
            pickTransition: workflow.pickTransition,
            transition: (id) => io.transition(auth, t.issueKey, id),
          },
        );
        outcome = { status: result.status, detail: result.detail };
        // "moved" and "no-transition" are settled; a failure is tried again.
        jobStore.patchData(job.id, {
          reviewMove: { moved: result.status !== "failed", status: result.status, detail: result.detail, at: Date.now() },
        });
      }

      const settled = outcome.status === "moved" || outcome.status === "no-transition" || outcome.status === "already";
      if (pr && settled) {
        const ref = safePr(pr);
        outcome.prId = pr.id;
        // Data first, then the status: the history records the job as its status changes.
        jobStore.patchData(job.id, { pr: ref });
        jobStore.update(job.id, {
          status: "approved",
          progress: undefined,
          result: {
            summary: `Pull request #${pr.id} is open. ${outcome.detail}`,
            files: [],
          },
        });
      }
      return outcome;
    } finally {
      inFlight.delete(job.id);
    }
  }

  async function tick(): Promise<number> {
    if (config.ticketToPr?.autoMoveToReview === false) return 0;
    let ended = 0;
    for (const job of jobStore.list()) {
      if (!trackable(job)) continue;
      try {
        const outcome = await check(job, {}, true);
        if (outcome.prId !== undefined) ended++;
        if (outcome.status === "failed") console.warn(`[ticket-to-pr] ${dataOf(job).issueKey}: ${outcome.detail}`);
        else if (outcome.status === "moved") console.log(`[ticket-to-pr] ${dataOf(job).issueKey}: ${outcome.detail}`);
      } catch (err) {
        console.warn(`[ticket-to-pr] review check failed: ${(err as Error).message}`);
      }
    }
    return ended;
  }

  return {
    tick,
    /** The panel's button: moves the ticket now, PR or not. */
    moveNow: (job: Job, auth: AuthContext) => check(job, auth, false),
    /** Starts the poll; returns how to stop it. */
    start(intervalMs = POLL_INTERVAL_MS): () => void {
      if (timer) return () => {};
      timer = setInterval(() => void tick(), intervalMs);
      timer.unref();
      return () => {
        if (timer) clearInterval(timer);
        timer = null;
      };
    },
  };
}
