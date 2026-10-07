export function buildResolveConflictPrompt(conflictedFiles: string[]): string {
  return `You are resolving git merge conflicts produced by merging the destination
branch into a pull request's source branch. The repository is already
checked out here with the merge in progress and conflict markers
(<<<<<<<, =======, >>>>>>>) present in the following files:

${conflictedFiles.map((f) => `- ${f}`).join("\n")}

For each file:
1. Open it and understand what changed on both sides of the conflict.
2. Resolve every conflict marker, preserving the intent of BOTH the pull
   request branch's changes and the destination branch's changes. Do not
   simply pick one side when both changes matter — integrate them.
3. Remove all conflict markers. When you are done there must be no
   "<<<<<<<", "=======", or ">>>>>>>" lines remaining anywhere in the repo.
4. If a quick build or test command is obviously available and fast to run,
   run it to sanity-check your resolution. Do not attempt a slow full test
   suite. Note: commands here run sandboxed and can only write inside this
   directory — a build/test step that needs to write elsewhere (e.g. a
   global cache like ~/.npm or ~/.gradle) is expected to fail for that
   reason, and is not itself a sign your resolution is wrong.

Do not run "git commit", "git push", or "git merge --abort" — leave the
merge staged. When finished, report which files you resolved and, for any
file where you made a judgment call between the two sides, briefly explain
the call you made.`;
}
