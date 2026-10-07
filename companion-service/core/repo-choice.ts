// The "Which repository?" question a running job can put to the user
// (job.data.pendingChoice, rendered by the extension's repo-choice buttons)
// and the answer that settles it. Shared by analyze-issue and ticket-to-pr.
import { Job, JobStatusError, jobStore } from "./jobs";

/** One repository offered in the question. */
export interface RepoOption {
  repoKey: string;
  reason: string;
}

/** `prompt` and `noRepoLabel` override the panel's default wording (written
 * for Analyze ticket). */
export interface RepoQuestion {
  suggested: string | null;
  options: RepoOption[];
  prompt?: string;
  noRepoLabel?: string;
}

/** Thrown into the waiting job when it is aborted. */
export class RepoChoiceCancelledError extends Error {}

/** Jobs waiting for the answer: the repos offered (held here, never taken
 * from the request) and how to deliver it. */
const pendingChoices = new Map<string, { offered: Set<string>; answer(repoKey: string | null): void }>();

const REPO_CHOICE_TIMEOUT_MS = 30 * 60 * 1000;

/** Publishes the question on the job and resolves with the repo the user
 * picked, or null for "no repo". Aborting the signal or no answer within half
 * an hour ends the wait. `retryHint` finishes the timeout message. */
export function askRepoChoice(
  jobId: string,
  signal: AbortSignal | undefined,
  question: RepoQuestion,
  retryHint: string,
): Promise<string | null> {
  return new Promise((resolve, reject) => {
    const finish = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
      pendingChoices.delete(jobId);
      try {
        jobStore.patchData(jobId, { pendingChoice: null });
      } catch {
        // The job is gone; nothing to clear.
      }
    };
    const onAbort = () => {
      finish();
      reject(new RepoChoiceCancelledError("cancelled"));
    };
    const timer = setTimeout(() => {
      finish();
      reject(new Error(`No answer to “Which repository?” within 30 minutes — ${retryHint}`));
    }, REPO_CHOICE_TIMEOUT_MS);
    timer.unref(); // an unanswered question never keeps the service from exiting
    signal?.addEventListener("abort", onAbort, { once: true });
    pendingChoices.set(jobId, {
      offered: new Set(question.options.map((o) => o.repoKey)),
      answer: (repoKey) => {
        finish();
        resolve(repoKey);
      },
    });
    jobStore.patchData(jobId, { pendingChoice: question });
  });
}

/** The feature action behind the buttons. Body { repoKey }: one of the repos
 * offered, or null. */
export function confirmRepoChoice(job: Job, body: unknown): unknown {
  const pending = pendingChoices.get(job.id);
  if (!pending) throw new JobStatusError("Nothing is waiting for a repository answer.");
  const repoKey = (body as { repoKey?: unknown } | undefined)?.repoKey;
  if (repoKey !== null && (typeof repoKey !== "string" || !pending.offered.has(repoKey))) {
    throw new JobStatusError("That isn't one of the repositories offered.");
  }
  pending.answer(repoKey);
  return jobStore.get(job.id);
}
