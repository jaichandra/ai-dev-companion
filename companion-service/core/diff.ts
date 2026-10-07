import { git } from "./exec";
import type { FileDiff } from "./jobs";

/**
 * Unified diff for a set of files, each against a fixed base ref, run
 * inside `worktreeDir`. `git diff` already produces a perfectly good
 * unified diff — no need for a diffing library on top of it.
 */
export async function computeFileDiffs(
  worktreeDir: string,
  baseRef: string,
  files: string[],
): Promise<FileDiff[]> {
  const diffs: FileDiff[] = [];
  for (const file of files) {
    const result = await git(["diff", baseRef, "--", file], worktreeDir);
    diffs.push({ path: file, diff: result.stdout });
  }
  return diffs;
}
