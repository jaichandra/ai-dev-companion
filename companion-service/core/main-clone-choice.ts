// The question a job puts to the user when the PR's branch is checked out in
// the main clone, where Claude may not edit: what to do about it. Rendered by
// the same panel buttons as "Which repository?" (core/repo-choice.ts).
import { askRepoChoice } from "./repo-choice";
import { MainCloneChoice, MainCloneInfo, WorkspaceUnusableError } from "./worktree";

export const WORKTREE_LABEL = "Move the clone off this branch and make a worktree for it";
export const DISPOSABLE_LABEL = "Use a temporary worktree";

/** Asks, and resolves with the choice; throws WorkspaceUnusableError for Cancel. */
export async function askMainCloneChoice(jobId: string, signal: AbortSignal | undefined, info: MainCloneInfo): Promise<MainCloneChoice> {
  const canSwitch = info.switchTo !== null;
  const worktreeOption = {
    repoKey: WORKTREE_LABEL,
    reason: canSwitch
      ? `runs git checkout ${info.switchTo} in the clone, then keeps a worktree for the branch at ${info.worktreeDir} that later jobs reuse`
      : `not possible: ${info.cannotSwitch}`,
  };
  const disposableOption = {
    repoKey: DISPOSABLE_LABEL,
    reason:
      "works on a throwaway copy of origin's branch and deletes it afterwards; the clone is not touched" +
      (info.ahead > 0 ? ` (its ${info.ahead} unpushed commit${info.ahead === 1 ? " is" : "s are"} not in that copy)` : ""),
  };
  const answer = await askRepoChoice(
    jobId,
    signal,
    {
      suggested: canSwitch ? WORKTREE_LABEL : DISPOSABLE_LABEL,
      options: canSwitch ? [worktreeOption, disposableOption] : [disposableOption],
      prompt:
        `The branch ${info.branch} is checked out in your main clone (${info.dir}), and a job that lets Claude edit can't ` +
        "run there. Nothing is changed until you answer. Cancel if you would rather switch the clone to another branch yourself.",
      noRepoLabel: "Cancel",
    },
    "start the job again.",
  );
  if (answer === null) throw new WorkspaceUnusableError("Cancelled: nothing was changed.");
  return answer === WORKTREE_LABEL ? "worktree" : "disposable";
}
