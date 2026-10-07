import { randomUUID } from "crypto";
import * as fs from "fs";
import * as path from "path";
import type { Config } from "../../config";
import { repoPath as resolveRepoPath } from "../../config";
import { git } from "../../core/exec";
import { runClaude } from "../../core/claude";
import {
  ClaudeSession,
  Feature,
  FileDiff,
  Job,
  JobStatusError,
  McpToolContext,
  WorktreeGoneError,
  jobStore,
} from "../../core/jobs";
import { fetchOpenPullRequest, pendingStartResult } from "../../core/mcp";
import { addWorktree, removeWorktree, WorktreeHandle } from "../../core/worktree";
import { assertTreeMatchesReviewed, canReject, stageAll, withJobLock, writeTree } from "../../core/reviewed-push";
import { buildResolveConflictPrompt } from "./prompt";

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
const plan = require("./plan.js") as {
  intersectNulSeparated(aZ: string, bZ: string): string[];
  shouldSkipCommit(opts: { stagedClean: boolean; aheadCount: number; isAncestor: boolean }): boolean;
  isNothingToPush(opts: { stagedClean: boolean; aheadCount: number }): boolean;
  classifyConflictedFile(opts: {
    path: string;
    isEmptyVsPreMerge: boolean;
    equalsDest: boolean;
  }): { path: string; diffAgainst: "preMerge" | "dest"; note?: string };
};

interface ResolveConflictPayload {
  project: string;
  repo: string;
  prId: number | string;
  /** The PR's branch — what we merge INTO and eventually push back to. */
  sourceBranch: string;
  /** Usually "master" — what we merge FROM. */
  destBranch: string;
}

interface JobData {
  repoPath: string;
  sourceBranch: string;
  worktree: WorktreeHandle;
  preMergeSha: string;
  /** `origin/<destBranch>`'s tip, recorded right before the merge attempt
   * — the merge's other parent. Needed (alongside preMergeSha) to compute
   * reviewFiles: a file only counts as needing review if its content in
   * the current tree differs from BOTH parents. */
  destSha: string;
  /** The /start payload, carried through untouched by every jobStore
   * .patchData call below — approve() reads payload.prId for the commit
   * message it writes when there's no in-progress merge to --no-edit. */
  payload: ResolveConflictPayload;
  /** Files that had conflict markers when the merge step ran; empty when
   * the merge succeeded cleanly. Set exactly once, immediately after the
   * merge (before Claude ever runs), and never touched again by anything
   * — approve()/refreshDiff() always read it as `data.conflicted ?? []`.
   * It answers "what did the merge itself conflict on", which is a fact
   * about that one merge and doesn't change after the fact; reviewFiles()
   * is what tracks whatever the worktree looks like *now*. */
  conflicted: string[];
  /** The headless `claude` run that resolved the conflict — lets a later
   * phase `--resume` it from the same worktree (see core/jobs.ts's
   * ClaudeSession). Absent for the "merge succeeded with no conflicts"
   * path, since Claude never ran. */
  claudeSession?: ClaudeSession;
  /** `git write-tree` of the whole worktree at the moment its diff was
   * last shown to the user (set at the end of runJob's diff step, and
   * again by every successful refreshDiff). approve() recomputes this
   * fingerprint right before pushing and refuses if it doesn't match —
   * the worktree changed (e.g. more edits in a resumed terminal) since
   * what the panel rendered. */
  reviewedTree?: string;
  /** The headless claude's pid (= its process-group id; core/exec.ts
   * starts it detached) while it runs, cleared as soon as runClaude
   * settles. Still set after a restart means that run was cut off — by a
   * crash or `kill -9`, which skip server.ts's shutdown handler — and that
   * claude may still be writing the worktree, so refreshDiff/approve
   * refuse while its group is alive (see assertNoLiveClaude). */
  claudePid?: number;
  /** Set (and the job moved to "failed") when core/worktree-integrity.js
   * detects that the worktree's `.git` gitlink or its private metadata in
   * the main repo (`commondir`/`gitdir`/`config.worktree` under `gitDir`)
   * changed since runJob's snapshot (taken as soon as the worktree
   * exists) — or when that snapshot is missing altogether, which is
   * treated as untrusted too (see assertGitMetadataIntact). Once set,
   * NOTHING in this file may run `git` in or against this worktree
   * again: the metadata that decides how git behaves here (e.g.
   * `core.hooksPath`) is no longer trusted. reject() (via
   * discardWorktree) removes the worktree with plain `fs`, not
   * `git worktree remove`; refreshDiff()/approve() refuse outright. */
  gitMetadataCompromised?: boolean;
  /** Why gitMetadataCompromised was set: "missing" (no baseline to compare
   * against — e.g. lost in a restart) or "changed" (the metadata really
   * differs). Only picks the message shown (untrustedMessage); the flag
   * above stays the gate every check keys on. A job flagged before this
   * field existed has none, and counts as "changed". */
  gitMetadataUntrusted?: "missing" | "changed";
}

/** Running jobs by id: aborting the controller kills the current git/claude
 * step, and `done` settles once runJob has stopped touching the worktree. */
const running = new Map<string, { controller: AbortController; done: Promise<void> }>();

class JobCancelledError extends Error {}

const COMPROMISED_MESSAGE = "Claude modified this worktree's git metadata, so it was not trusted. Discard the job.";

const MISSING_BASELINE_MESSAGE =
  "This job's worktree-integrity record is missing (it may have been lost in a service restart), so its worktree isn't trusted. Discard the job and start again.";

const CLAUDE_STILL_RUNNING_MESSAGE =
  "Claude from before the service restarted may still be running in this worktree. Wait a minute and click Refresh diff again, or Discard the job.";

/** Called first thing in refreshDiff/approve, before the integrity checks
 * and before any git: a `claudePid` left over from before a restart whose
 * process group is still alive means an orphaned claude may be writing
 * this worktree right now, so staging, committing or pushing it would act
 * on a half-finished edit that no post-run compare has seen. Refuses
 * rather than kills — after a restart that pid may belong to an unrelated
 * process. Discard is still allowed. Once the group is gone the stale pid
 * is cleared, so a later reuse of that number can't refuse this job. */
function assertNoLiveClaude(jobId: string, data: { claudePid?: number }): void {
  if (data.claudePid === undefined) return;
  if (isProcessGroupAlive(data.claudePid)) throw new JobStatusError(CLAUDE_STILL_RUNNING_MESSAGE);
  jobStore.patchData(jobId, { claudePid: undefined });
}

/** The git-metadata baseline for each job lives in the shared, persisted
 * store (core/integrity-baselines.js) — the same one
 * address-review-comments uses. runJob takes it as soon as the worktree
 * exists (before the merge, and long before Claude runs), compares
 * against it right after the Claude run, and refreshDiff/approve/discard
 * compare again before any git they run, since a resumed Claude Code
 * session (Continue in Claude Code) can change the worktree after that
 * first compare. Every job that has a worktree has a baseline, so a
 * missing one (lost in a restart with persistence off, or an unreadable
 * file) is never read as "nothing to compare against": it's untrusted. */

/** True only if the worktree's git metadata still matches the baseline.
 * A missing baseline, or an unreadable file, counts as not intact. */
function gitMetadataIntact(jobId: string, worktreeDir: string): boolean {
  return baselines.check(jobId, worktreeDir) === "intact";
}

/** The message for a job flagged untrusted: a missing baseline isn't
 * evidence Claude did anything, so it mustn't be reported as tampering.
 * No recorded reason (a job flagged before gitMetadataUntrusted existed)
 * reads as "changed". */
function untrustedMessage(data: { gitMetadataUntrusted?: "missing" | "changed" }): string {
  return data.gitMetadataUntrusted === "missing" ? MISSING_BASELINE_MESSAGE : COMPROMISED_MESSAGE;
}

/** Why a worktree that isn't intact isn't: "missing" only when there's no
 * baseline at all; anything else not intact is "changed". */
function untrustedReason(jobId: string, worktreeDir: string): "missing" | "changed" {
  return baselines.check(jobId, worktreeDir) === "missing" ? "missing" : "changed";
}

/** Called before the git refreshDiff and approve run: a mismatch or a
 * missing baseline marks the job compromised (see
 * JobData.gitMetadataCompromised), with the reason, and throws before any
 * git runs. It also moves the job to "failed", so the panel stops offering
 * Approve and leaves only Discard — refreshDiff's refusal would otherwise
 * leave an "awaiting-approval" job looking approvable. */
function assertGitMetadataIntact(jobId: string, worktreeDir: string): void {
  const state = baselines.check(jobId, worktreeDir);
  if (state === "intact") return;
  jobStore.patchData(jobId, { gitMetadataCompromised: true, gitMetadataUntrusted: state });
  const msg = untrustedMessage({ gitMetadataUntrusted: state });
  jobStore.update(jobId, { status: "failed", error: msg });
  throw new JobStatusError(msg);
}

/** Merge --abort and remove the job's worktree, if it got that far. The
 * worktree is detached and never pushed, so this undoes everything. */
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
    // Claude may have redirected git itself (core.hooksPath, a rewritten
    // gitlink, etc — see JobData.gitMetadataCompromised), or there's no
    // baseline left to tell either way — do not invoke
    // `git` in or against this worktree at all, not even
    // `git worktree remove` from the (trusted) main repo, since that
    // reads the very metadata (`<gitDir>/gitdir`) that may have been
    // tampered with. Plain fs removal only. This deliberately leaves the
    // main repo's own `.git/worktrees/<id>` registration behind (rather
    // than risk a `git worktree prune`/`remove` touching it) — a small,
    // inert leftover, safe to clean up by hand later.
    fs.rmSync(data.worktree.dir, { recursive: true, force: true });
    return;
  }
  await git(["merge", "--abort"], data.worktree.dir).catch(() => undefined);
  await removeWorktree(data.worktree);
}

const CONFLICT_MARKER_RE = /^(<{7}|={7}|>{7})(?!=)/m;

function assertPayload(body: unknown): ResolveConflictPayload {
  const p = body as Partial<ResolveConflictPayload> | null;
  if (
    !p ||
    typeof p.project !== "string" ||
    typeof p.repo !== "string" ||
    typeof p.sourceBranch !== "string" ||
    typeof p.destBranch !== "string"
  ) {
    throw new Error(
      "resolve-conflict payload must include project, repo, sourceBranch, destBranch (strings)",
    );
  }
  return p as ResolveConflictPayload;
}

/** Files (relative to the worktree) that still contain conflict markers. */
function filesWithConflictMarkers(worktreeDir: string, files: string[]): string[] {
  const bad: string[] = [];
  for (const file of files) {
    const full = path.join(worktreeDir, file);
    if (!fs.existsSync(full)) continue; // a resolved conflict may delete the file
    const content = fs.readFileSync(full, "utf8");
    if (CONFLICT_MARKER_RE.test(content)) {
      bad.push(file);
    }
  }
  return bad;
}

/** Files whose content in `tree` differs from BOTH merge parents — git's
 * own "combined diff" notion, computed by intersecting two tree-vs-tree
 * diffs (`git diff <treeish> <treeish>` never touches the working
 * directory; preMergeSha/destSha/tree are all plain tree-ish objects).
 * Content that's purely from the source branch, or purely from the
 * destination branch, differs from only ONE parent and is excluded —
 * there's nothing to review there, since nothing was actually combined.
 * A conflict's resolution, an auto-merged file, or a stray new file all
 * differ from both and are included. */
async function reviewFiles(worktreeDir: string, tree: string, preMergeSha: string, destSha: string): Promise<string[]> {
  // --no-renames: a rename would otherwise report as a single "R" entry
  // with BOTH the old and new path folded into one name-only line's
  // meaning — that's not a NUL-delimited pair of independent paths this
  // intersection can reason about; treating it as a plain delete+add
  // keeps every path here a real, standalone file.
  const fromSource = (await git(["diff", "--no-renames", "--name-only", "-z", preMergeSha, tree], worktreeDir))
    .stdout;
  const fromDest = (await git(["diff", "--no-renames", "--name-only", "-z", destSha, tree], worktreeDir)).stdout;
  return plan.intersectNulSeparated(fromSource, fromDest);
}

type RenderOutcome =
  | { ok: true; tree: string; files: string[]; fileDiffs: FileDiff[] }
  | { ok: false; markers: string[] };

/** Builds the panel's FileDiff[] for `panelFiles` (= reviewFiles(tree) ∪
 * conflicted — see renderReview). A conflicted file resolved wholesale to
 * one side needs special handling: it may not differ from preMergeSha at
 * all (kept the PR branch's own content, so it wouldn't even be IN
 * reviewFiles(tree) — that's exactly why `conflicted` is unioned in
 * rather than relied on to appear there naturally), in which case a plain
 * diff against preMergeSha would be empty and would hide that the
 * destination branch's changes were dropped. plan.classifyConflictedFile
 * decides, per file, which base to diff against and what note (if any) to
 * attach — see FileDiff.note in core/jobs.ts. */
async function buildPanelFileDiffs(
  worktreeDir: string,
  preMergeSha: string,
  destSha: string,
  panelFiles: string[],
  conflicted: string[],
): Promise<FileDiff[]> {
  const conflictedSet = new Set(conflicted);
  const out: FileDiff[] = [];
  for (const file of panelFiles) {
    const preMergeDiff = (await git(["diff", preMergeSha, "--", file], worktreeDir)).stdout;
    if (!conflictedSet.has(file)) {
      out.push({ path: file, diff: preMergeDiff });
      continue;
    }
    const destDiff = (await git(["diff", destSha, "--", file], worktreeDir)).stdout;
    const decision = plan.classifyConflictedFile({
      path: file,
      isEmptyVsPreMerge: preMergeDiff === "",
      equalsDest: destDiff === "",
    });
    out.push({
      path: file,
      diff: decision.diffAgainst === "dest" ? destDiff : preMergeDiff,
      ...(decision.note ? { note: decision.note } : {}),
    });
  }
  return out;
}

/** The one recipe runJob (both the clean-merge and the conflict-resolved
 * path) and refreshDiff all share: stage everything, fingerprint the
 * tree, compute reviewFiles(tree), and marker-check the PANEL set —
 * reviewFiles(tree) ∪ conflicted (the files that had conflict markers),
 * never every file the merge brought in, which would flag markers in
 * destination-branch content this job never touched, and never
 * reviewFiles(tree) alone, which would miss a conflicted file resolved
 * wholesale to one side (see buildPanelFileDiffs). On success this also
 * renders the diff the panel shows; on markers-remain it doesn't (there's
 * nothing valid to show yet). */
async function renderReview(
  worktreeDir: string,
  preMergeSha: string,
  destSha: string,
  conflicted: string[],
): Promise<RenderOutcome> {
  await stageAll(worktreeDir);
  const tree = await writeTree(worktreeDir);
  const combined = await reviewFiles(worktreeDir, tree, preMergeSha, destSha);
  const panelFiles = [...new Set([...combined, ...conflicted])];
  const markers = filesWithConflictMarkers(worktreeDir, panelFiles);
  if (markers.length > 0) {
    return { ok: false, markers };
  }
  return {
    ok: true,
    tree,
    files: panelFiles,
    fileDiffs: await buildPanelFileDiffs(worktreeDir, preMergeSha, destSha, panelFiles, conflicted),
  };
}

export function createResolveConflictFeature(config: Config): Feature {
  return {
    id: "resolve-conflict",
    label: "Resolve Conflict",

    async start(payloadRaw: unknown): Promise<Job> {
      const payload = assertPayload(payloadRaw);
      const repo = resolveRepoPath(config, payload.project, payload.repo);
      const job = jobStore.create("resolve-conflict", { payload });
      const controller = new AbortController();

      // Deliberately not awaited by the caller — the HTTP handler returns
      // the (running) job immediately and the extension polls /status.
      const done = runJob(job.id, repo, payload, config, controller.signal)
        .catch(async (err: Error) => {
          if (!controller.signal.aborted) {
            jobStore.update(job.id, { status: "failed", error: err.message });
            return;
          }
          await discardWorktree(job).catch((cleanupErr: Error) =>
            console.warn(`[resolve-conflict] cleanup after cancel failed: ${cleanupErr.message}`),
          );
          baselines.remove(job.id);
          jobStore.update(job.id, { status: "rejected", progress: undefined });
        })
        .finally(() => running.delete(job.id));
      running.set(job.id, { controller, done });

      return job;
    },

    async cancel(job: Job): Promise<void> {
      // Locked for the same reason approve()/refreshDiff() are: without
      // it, this could read a stale "running" status (captured by
      // server.ts before this call) and stamp "rejecting" over a job that
      // has, by the time this actually runs, already moved on to
      // "awaiting-approval" — clobbering a concurrent refreshDiff/approve's
      // work on the same worktree.
      await withJobLock(job.id, async () => {
        const current = jobStore.get(job.id);
        const entry = running.get(job.id);
        if (!entry || !current || current.status !== "running") {
          throw new Error(`Cannot cancel job in status "${current?.status ?? job.status}"`);
        }
        jobStore.update(job.id, {
          status: "rejecting",
          progress: { stepId: "cancel", label: "Cancelling and discarding the merge…" },
        });
        entry.controller.abort(new JobCancelledError("cancelled"));
        await entry.done;
      });
    },

    async approve(job: Job): Promise<void> {
      await withJobLock(job.id, async () => {
        // Re-read: this call may have been queued behind another
        // approve()/refreshDiff() for the same job, so the `job` object
        // captured before the lock can be stale by the time it's our turn.
        const current = jobStore.get(job.id);
        if (!current || current.status !== "awaiting-approval") {
          throw new Error(`Cannot approve job in status "${current?.status ?? "unknown"}"`);
        }
        const data = current.data as unknown as JobData;
        assertNoLiveClaude(job.id, data);
        // Defense in depth: approve() can only normally be reached from
        // "awaiting-approval", which a compromised job is moved out of
        // (see runJob's snapshot/compare) — but never run git here on
        // the strength of that alone.
        if (data.gitMetadataCompromised) {
          throw new JobStatusError(untrustedMessage(data));
        }
        jobStore.update(job.id, { status: "approving" });
        try {
          assertGitMetadataIntact(job.id, data.worktree.dir);
          await stageAll(data.worktree.dir);
          const tree = await writeTree(data.worktree.dir);

          // Approve must push exactly what the panel last showed (see
          // core/reviewed-push.ts's assertTreeMatchesReviewed).
          assertTreeMatchesReviewed(tree, data.reviewedTree);

          // Belt and suspenders: tree === data.reviewedTree means this is
          // byte-for-byte the same content already marker-checked clean
          // when that fingerprint was stamped (see renderReview) — but
          // check again rather than trust that invariant blindly.
          const reviewSet = await reviewFiles(data.worktree.dir, tree, data.preMergeSha, data.destSha);
          const stillConflicted = filesWithConflictMarkers(data.worktree.dir, [
            ...new Set([...reviewSet, ...(data.conflicted ?? [])]),
          ]);
          if (stillConflicted.length > 0) {
            throw new Error(
              `Conflict markers remain in: ${stillConflicted.join(", ")}. Resolve them (e.g. in the ` +
                `worktree at ${data.worktree.dir}) and try again — Refresh diff picks up the fix.`,
            );
          }

          const stagedClean =
            (await git(["diff", "--cached", "--quiet"], data.worktree.dir, { allowFailure: true })).code === 0;
          const aheadCount = Number(
            (await git(["rev-list", "--count", `${data.preMergeSha}..HEAD`], data.worktree.dir)).stdout.trim(),
          );

          if (plan.isNothingToPush({ stagedClean, aheadCount })) {
            // The merge was "Already up to date" (or produced no real
            // change) and nothing else changed either — there's nothing to
            // commit AND nothing to push.
            assertGitMetadataIntact(job.id, data.worktree.dir);
            await removeWorktree(data.worktree);
            baselines.remove(job.id);
            jobStore.update(job.id, {
              status: "approved",
              result: {
                ...(current.result ?? { files: [] }),
                summary: "Nothing to push — the branch is already up to date.",
              },
            });
            return;
          }

          // Nothing staged relative to HEAD, HEAD has moved past
          // preMergeSha, and that move is real ancestry (not just "some
          // commits differ") means there's no pending commit to make —
          // either `git merge` itself auto-committed a clean, conflict-free
          // merge, or the user ran `git commit` themselves in a resumed
          // terminal session. Either way, `git commit` here would just fail
          // with "nothing to commit".
          const isAncestor =
            (
              await git(["merge-base", "--is-ancestor", data.preMergeSha, "HEAD"], data.worktree.dir, {
                allowFailure: true,
              })
            ).code === 0;

          if (!plan.shouldSkipCommit({ stagedClean, aheadCount, isAncestor })) {
            assertGitMetadataIntact(job.id, data.worktree.dir);
            const hasMergeHead =
              (
                await git(["rev-parse", "-q", "--verify", "MERGE_HEAD"], data.worktree.dir, { allowFailure: true })
              ).code === 0;
            if (hasMergeHead) {
              // --no-verify: skip the repo's pre-commit hook for this
              // commit. The human review that matters already happened —
              // approving the diff in the extension panel. Explicitly
              // decided with the user after hitting a real case where
              // husky/lint-staged failed on pre-existing lint debt in
              // unrelated files pulled in from the destination branch, not
              // on anything this tool (or the PR) touched. Hooks still run
              // normally for any commit the user makes themselves.
              await git(["commit", "--no-edit", "--no-verify"], data.worktree.dir);
            } else {
              // No merge in progress to --no-edit: the user already
              // committed the merge itself in a resumed terminal session,
              // and these are further review edits staged on top of that.
              // Name the commit explicitly instead.
              await git(
                ["commit", "-m", `Resolve review edits for PR #${data.payload.prId}`, "--no-verify"],
                data.worktree.dir,
              );
            }
          }
          assertGitMetadataIntact(job.id, data.worktree.dir);
          await git(["push", "origin", `HEAD:${data.sourceBranch}`], data.worktree.dir);
          await removeWorktree(data.worktree);
          baselines.remove(job.id);
          jobStore.update(job.id, { status: "approved" });
        } catch (err) {
          // Leave the worktree in place on failure (e.g. push rejected, or
          // markers still present) so the user can inspect or finish it
          // manually rather than losing the work.
          jobStore.update(job.id, { status: "failed", error: (err as Error).message });
          throw err;
        }
      });
    },

    async reject(job: Job): Promise<void> {
      // Fast-path refusal before even queuing for the lock: if the job is
      // already mid-flight on approve/reject, say so immediately instead
      // of making the caller wait for the lock just to be refused anyway.
      // This is advisory only — the job's status can still change between
      // this check and actually taking the lock below, which is why the
      // lock re-reads and re-checks rather than trusting this.
      if (job.status === "approving" || job.status === "rejecting") {
        throw new JobStatusError(`This job is already ${job.status}.`);
      }
      // Locked so a Discard click can't remove the worktree out from under
      // an in-flight approve()/refreshDiff() for the same job — it queues
      // behind whichever of those is already running instead. Once we have
      // the lock, re-read the job's status rather than trust what was
      // captured above: approve() holds this same lock for its whole
      // duration, so by the time reject() gets in here, a queued-behind
      // approve() has already finished — landing on "approved" (pushed) or
      // "failed" — and reject() must not clobber "approved" with
      // "rejecting"/"rejected" just because the caller queued the discard
      // before the approve finished.
      await withJobLock(job.id, async () => {
        const current = jobStore.get(job.id);
        if (!current) {
          throw new JobStatusError(`Unknown job id: ${job.id}`);
        }
        if (!canReject(current.status)) {
          throw new JobStatusError(`This job is already ${current.status}.`);
        }
        jobStore.update(job.id, { status: "rejecting" });
        await discardWorktree(current);
        baselines.remove(job.id);
        jobStore.update(job.id, { status: "rejected" });
      });
    },

    async refreshDiff(job: Job): Promise<Job> {
      return withJobLock(job.id, async () => {
        // Re-read for the same reason approve() does: this call may have
        // been queued behind another approve()/refreshDiff() for this job.
        const current = jobStore.get(job.id);
        if (!current) {
          throw new JobStatusError(`Unknown job id: ${job.id}`);
        }
        if (current.status !== "awaiting-approval" && current.status !== "failed") {
          throw new JobStatusError(`Cannot refresh the diff for a job in status "${current.status}"`);
        }
        const data = current.data as unknown as JobData;
        // A job reconciled to "failed" after a crash is exactly what this
        // accepts, and its claude may have outlived the old process.
        assertNoLiveClaude(job.id, data);
        // Load-bearing (unlike approve()'s copy of this check): a
        // compromised job IS "failed", which refreshDiff() otherwise
        // accepts — and running the diff-refresh's git commands (stageAll,
        // write-tree, diff) here is exactly the "next git command in this
        // worktree" the whole check exists to stop. See runJob's
        // snapshot/compare and JobData.gitMetadataCompromised.
        if (data.gitMetadataCompromised) {
          throw new JobStatusError(untrustedMessage(data));
        }
        if (!data.worktree || !fs.existsSync(data.worktree.dir)) {
          throw new WorktreeGoneError(
            "The worktree for this job no longer exists (it may already have been approved or discarded).",
          );
        }

        // "Nothing to review" is specifically the merge-failed-outright
        // case (runJob's non-conflict failure branch): the job is FAILED
        // (not e.g. an "awaiting-approval" job whose merge was legitimately
        // "Already up to date", which also has an empty `conflicted`, no
        // MERGE_HEAD, and HEAD still at preMergeSha, and must stay
        // refreshable), no conflict markers were ever found, no merge is
        // in progress, and HEAD never moved — there's no diff this could
        // ever refresh into.
        const conflicted = data.conflicted ?? [];
        assertGitMetadataIntact(job.id, data.worktree.dir);
        const headSha = (await git(["rev-parse", "HEAD"], data.worktree.dir)).stdout.trim();
        const hasMergeHead =
          (await git(["rev-parse", "-q", "--verify", "MERGE_HEAD"], data.worktree.dir, { allowFailure: true }))
            .code === 0;
        if (
          current.status === "failed" &&
          conflicted.length === 0 &&
          !hasMergeHead &&
          headSha === data.preMergeSha
        ) {
          throw new JobStatusError(
            "There's nothing to refresh — the merge failed before any conflict was ever found, so no diff was produced.",
          );
        }

        const outcome = await renderReview(data.worktree.dir, data.preMergeSha, data.destSha, conflicted);
        if (!outcome.ok) {
          return jobStore.update(job.id, {
            status: "failed",
            error: `Conflict markers remain in: ${outcome.markers.join(", ")}.`,
          });
        }
        jobStore.patchData(job.id, { reviewedTree: outcome.tree });
        return jobStore.update(job.id, {
          status: "awaiting-approval",
          error: undefined,
          result: {
            summary:
              outcome.files.length === 0
                ? "Refreshed — nothing needed resolving."
                : outcome.files.length === 1
                  ? "Refreshed diff — 1 file to review. Review below before approving."
                  : `Refreshed diff — ${outcome.files.length} files to review. Review below before approving.`,
            files: outcome.fileDiffs,
          },
        });
      });
    },

    // Only ever queues: this feature's Claude run can edit and its approve
    // pushes, so the job waits as "pending-start" for the user's Start
    // click on the PR page (server.ts's POST /jobs/:jobId/start).
    mcpTools() {
      return [
        {
          name: "start_resolve_conflict",
          async handler(args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
            const project = args.project as string;
            const repo = args.repo as string;
            const prId = args.prId as number;
            const pr = await fetchOpenPullRequest(config, ctx.auth, project, repo, prId);
            const job = ctx.createPendingJob("resolve-conflict", {
              project,
              repo,
              prId,
              sourceBranch: pr.fromBranch,
              destBranch: pr.toBranch,
            });
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
  payload: ResolveConflictPayload,
  config: Config,
  signal: AbortSignal,
): Promise<void> {
  // Called before every status/progress update, so a cancelled job never
  // moves on to the next step or overwrites its "rejecting" state.
  const checkpoint = () => {
    if (signal.aborted) throw new JobCancelledError("cancelled");
  };

  jobStore.update(jobId, {
    progress: { stepId: "fetch", label: "Fetching latest changes and preparing an isolated workspace…" },
  });
  const worktree = await addWorktree(repoPath, payload.sourceBranch, jobId);
  // Recorded before anything else can throw, so cancel/reject can find it.
  jobStore.patchData(jobId, { worktree });
  checkpoint();
  // `git add`/`git commit` inside a worktree write to the worktree's
  // index/HEAD, which git keeps in the MAIN repo, not the worktree
  // checkout — `git rev-parse --absolute-git-dir` run from inside the
  // worktree resolves that (`<main repo>/.git/worktrees/<id>`) so the
  // Claude step below can explicitly allow-write it, and the baseline
  // can snapshot the private metadata that lives there.
  const gitDir = (await git(["rev-parse", "--absolute-git-dir"], worktree.dir)).stdout.trim();
  // Snapshotted as soon as the worktree exists — before the merge, and
  // before any Claude run — so every job with a worktree has a baseline,
  // including a clean merge that never reaches Claude. Only the service's
  // own git runs between here and Claude, and none of it writes the
  // snapshotted files. Compared right after the Claude run and again
  // before every later git; see core/integrity-baselines.js and
  // JobData.gitMetadataCompromised. Throws (failing the job before Claude
  // runs) if persistence is on and the baseline can't be written.
  baselines.take(jobId, { worktreeDir: worktree.dir, gitDir });
  const preMergeSha = (await git(["rev-parse", "HEAD"], worktree.dir)).stdout.trim();
  // Everything else runJob's job.data ever needs is static from here on —
  // patched once so every later status/progress update (see the sites
  // below) can update just the fields that are actually changing without
  // dropping `payload` (set at jobStore.create) or `claudeSession` (set
  // once Claude has run) the way a wholesale `data: {...}` replace would.
  jobStore.patchData(jobId, { repoPath, sourceBranch: payload.sourceBranch, preMergeSha });

  checkpoint();
  jobStore.update(jobId, {
    progress: { stepId: "merge", label: `Merging origin/${payload.destBranch} in…` },
  });

  // The merge's other parent — recorded right before the merge attempt so
  // it's captured regardless of how the merge turns out. Needed alongside
  // preMergeSha to compute reviewFiles later (a file only needs review if
  // it differs from BOTH parents).
  const destSha = (await git(["rev-parse", `origin/${payload.destBranch}`], worktree.dir)).stdout.trim();
  jobStore.patchData(jobId, { destSha });

  // A conflicting merge exits non-zero — that's the expected path here, so
  // don't let it reject the promise; conflicts are detected explicitly
  // below. allowFailure only suppresses the throw, not the exit code: a
  // merge can also fail for reasons that are NOT a normal conflict (e.g.
  // unrelated histories from a shallow/broken clone, a bad ref) — those
  // must not be mistaken for "no conflicts found", so mergeResult.code is
  // checked below alongside the conflicted-file list.
  const mergeResult = await git(
    ["merge", "--no-edit", `origin/${payload.destBranch}`],
    worktree.dir,
    { allowFailure: true, signal },
  );

  const conflicted = (
    await git(["diff", "--name-only", "--diff-filter=U"], worktree.dir, { signal })
  ).stdout
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
  // Set once, immediately, before Claude ever runs — approve()/refreshDiff()
  // read this as `data.conflicted ?? []` for the rest of the job's life and
  // never touch it again themselves (see JobData's doc comment).
  jobStore.patchData(jobId, { conflicted });

  checkpoint();
  if (conflicted.length === 0 && mergeResult.code !== 0) {
    // The merge failed, but not with the usual conflict markers — e.g. "not
    // something to git merge" (unrelated histories, missing ref). Treating
    // this as "no conflicts, nothing to review" would hide a real failure;
    // surface it instead.
    jobStore.update(jobId, {
      status: "failed",
      error:
        `git merge failed (not with the usual conflict markers): ` +
        (mergeResult.stderr.trim() || mergeResult.stdout.trim() || `exit code ${mergeResult.code}`),
    });
    return;
  }

  if (conflicted.length === 0) {
    // Merge succeeded cleanly (exit 0) — Bitbucket's conflict flag can be
    // stale, or the merge resolved on its own — either way there's nothing
    // for Claude to do. The merge is already committed by `git merge`.
    jobStore.update(jobId, {
      progress: { stepId: "diff", label: "Preparing diff for review…" },
    });
    const outcome = await renderReview(worktree.dir, preMergeSha, destSha, conflicted);
    checkpoint();
    if (!outcome.ok) {
      jobStore.update(jobId, {
        status: "failed",
        error: `Conflict markers remain in: ${outcome.markers.join(", ")}.`,
      });
      return;
    }
    jobStore.patchData(jobId, { reviewedTree: outcome.tree });
    jobStore.update(jobId, {
      status: "awaiting-approval",
      result: {
        summary:
          outcome.files.length === 0
            ? "Merged cleanly — nothing needed resolving. Approving pushes the merge commit."
            : "No merge conflicts were found (the merge either succeeded cleanly or was " +
              "already up to date). Review below and approve to push, or discard to throw it away.",
        files: outcome.fileDiffs,
      },
    });
    return;
  }

  jobStore.update(jobId, {
    progress: {
      stepId: "resolve",
      label:
        conflicted.length === 1
          ? `Resolving "${conflicted[0]}" with Claude — this can take a few minutes…`
          : `Resolving ${conflicted.length} conflicted files with Claude — this can take a few minutes…`,
    },
  });
  // Own session id (rather than relying solely on the CLI's echoed-back
  // session_id) so claudeSession.id is always populated even if
  // --output-format json fails to parse (see core/claude.ts's runClaude) —
  // and so a later phase can --resume this exact transcript in worktree.dir.
  const sessionId = randomUUID();
  // worktreeWrite now runs under the Bash sandbox (core/claude-args.js's
  // isSandboxedPolicy), which allows writes under `cwd` (worktree.dir) by
  // default but nowhere else — plus `gitDir` (resolved right after
  // addWorktree above), which git needs for the worktree's index/HEAD.

  // allowWrite: [gitDir] above is necessarily broad (git needs to write
  // its index, HEAD, etc there) — but `<gitDir>/commondir`, `<gitDir>/gitdir`
  // and `<gitDir>/config.worktree`, plus the worktree's own `.git` gitlink
  // FILE (inside cwd, so allowed by default), can each redirect how git
  // itself behaves on its NEXT invocation (core.hooksPath, core.fsmonitor,
  // etc) — exactly what the service's own next `git add -A` would then
  // run. Denying them wins over the broader allowWrite (verified live:
  // Claude Code's sandbox applies denyWrite after allowWrite).
  const sandboxDenyWrite = [
    path.join(worktree.dir, ".git"),
    path.join(gitDir, "commondir"),
    path.join(gitDir, "gitdir"),
    path.join(gitDir, "config.worktree"),
  ];

  // Edit/Write are separate tools the Bash sandbox doesn't govern at all
  // (live-verified: a Write outside the sandboxed cwd succeeded with
  // sandbox.filesystem.denyWrite set and no denial). Confining them was
  // FIRST attempted with plain `--disallowedTools` permission-rule
  // strings (Edit(//<home>/**) etc) — live-verified to be a regression in
  // production: once the worktree lives under $HOME (as it does there,
  // `~/.ai-dev-companion/worktrees/...`), that broad a deny also
  // matches paths INSIDE the worktree, denying Claude's own edits to
  // files it needed to edit. `editRoot` (core/edit-guard.js, a
  // PreToolUse hook — see core/claude-args.js's buildClaudeArgs) does
  // real filesystem resolution instead: allow only inside this one real
  // path, deny anything touching a `.git` segment even inside it.
  // `fs.realpathSync` (not the raw `worktree.dir`) so the hook's own
  // "inside root" check compares like with like if any component of the
  // state dir path is itself a symlink.
  const editRoot = fs.realpathSync(worktree.dir);

  // The baseline taken right after addWorktree is compared right after
  // this run — the actual fallback for whatever the above two mitigations
  // miss. See core/integrity-baselines.js and JobData.gitMetadataCompromised.

  // claudePid is persisted with the job while claude runs, and cleared
  // however the run ends — see JobData.claudePid.
  let result: Awaited<ReturnType<typeof runClaude>>;
  try {
    result = await runClaude({
      cwd: worktree.dir,
      prompt: buildResolveConflictPrompt(conflicted),
      policy: "worktreeWrite",
      sessionId,
      sandboxAllowWrite: [gitDir],
      sandboxDenyWrite,
      editRoot,
      signal,
      onSpawn: (pid) => jobStore.patchData(jobId, { claudePid: pid }),
      onProgress: (label) => jobStore.setProgressIfRunning(jobId, "resolve", label),
    });
  } finally {
    jobStore.patchData(jobId, { claudePid: undefined });
  }

  // Checked BEFORE anything else touches this worktree with git — a
  // change here (or a missing baseline, which gitMetadataIntact also
  // reports as not intact) means the metadata that decides how git
  // behaves in this worktree is no longer trustworthy, so no git command
  // (not even a read-only one) may run against it from this point on.
  if (!gitMetadataIntact(jobId, worktree.dir)) {
    const gitMetadataUntrusted = untrustedReason(jobId, worktree.dir);
    jobStore.patchData(jobId, { gitMetadataCompromised: true, gitMetadataUntrusted });
    jobStore.update(jobId, { status: "failed", error: untrustedMessage({ gitMetadataUntrusted }) });
    return;
  }

  const claudeSession: ClaudeSession = {
    id: result.sessionId || sessionId,
    cwd: worktree.dir,
    permissionMode: "default",
  };
  jobStore.patchData(jobId, { claudeSession });

  checkpoint();
  jobStore.update(jobId, {
    progress: { stepId: "verify", label: "Verifying the resolution…" },
  });
  jobStore.update(jobId, {
    progress: { stepId: "diff", label: "Preparing diff for review…" },
  });
  const outcome = await renderReview(worktree.dir, preMergeSha, destSha, conflicted);
  checkpoint();
  if (!outcome.ok) {
    jobStore.update(jobId, {
      status: "failed",
      error:
        `Claude did not fully resolve conflicts in: ${outcome.markers.join(", ")}. ` +
        `The worktree at ${worktree.dir} is left in place for manual inspection — ` +
        `discard the job to clean it up.`,
    });
    return;
  }
  jobStore.patchData(jobId, { reviewedTree: outcome.tree });
  jobStore.update(jobId, {
    status: "awaiting-approval",
    result: {
      summary: `Resolved ${conflicted.length} conflicted file(s) against origin/${payload.destBranch}. Review the diffs below before approving.`,
      files: outcome.fileDiffs,
    },
  });
}
