import * as fs from "fs";
import * as path from "path";
import type { Config } from "../../config";
import { repoPath as resolveRepoPath } from "../../config";
import { git } from "../../core/exec";
import { resolveReviewEditor, openForReview, EditorId } from "../../core/editor";
import { Feature, Job, jobStore, SessionStatus, StartRefusedError } from "../../core/jobs";
import type { HeaderSegment, TerminalLocation, TerminalOptions } from "../../core/terminal";

/** core/review-session.js's per-worktree record of the review using it. */
interface SessionRecord {
  jobId: string;
  worktree: string;
  payload: ReviewPayload;
  summary: string;
  review: Record<string, unknown>;
  terminal?: TerminalLocation;
}

interface LiveSession {
  record: SessionRecord;
  status: SessionStatus;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const reviewSession = require("../../core/review-session.js") as {
  createStatusFile(jobId: string): string;
  statusFileFor(jobId: string): string;
  readStatus(file: string): Promise<SessionStatus>;
  isLive(status: SessionStatus): boolean;
  stopSession(file: string): Promise<void>;
  waitUntilStopped(file: string): Promise<SessionStatus>;
  writeSessionRecord(record: SessionRecord): void;
  listSessionRecords(): SessionRecord[];
  removeSessionRecord(worktree: string, jobId: string): void;
  findLiveSession(worktree: string): Promise<LiveSession | null>;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const terminalFocus = require("../../core/terminal-focus.js") as {
  focusTerminal(loc: TerminalLocation): Promise<boolean>;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const reviewWorktree = require("../../core/review-worktree.js") as {
  reviewWorktreePath(repoPath: string): string;
  isRegisteredWorktree(repoPath: string, dir: string): Promise<boolean>;
  provisionReviewWorktree(repoPath: string): Promise<{ dir: string; created: boolean }>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  assertPayload(body: unknown): ReviewPayload;
  hasReviewSkill(editor: EditorId, dir: string): boolean;
  buildReviewPrompt(args: ReviewPayload & { useSkill: boolean }): string;
  buildTerminalHeader(args: ReviewPayload & { editor: EditorId; dir: string }): HeaderSegment[][];
  buildTerminalTitle(args: ReviewPayload): Required<Pick<TerminalOptions, "title" | "badge">>;
  parseDirtyFiles(porcelainStdout: string): string[];
  buildSummary(args: {
    branch: string;
    dir: string;
    editorLabel?: string;
    stashedFiles: string[];
    reviewNote?: string;
    usedSkill?: boolean;
  }): string;
  buildReviewResult(args: {
    payload: ReviewPayload;
    dir: string;
    editor: EditorId;
    reviewStarted: boolean;
    reviewNote?: string;
    usedSkill: boolean;
    stashedFiles: string[];
    tracked: boolean;
  }): Record<string, unknown>;
  parseReplaceJobId(body: unknown): string | undefined;
  decideStart(args: {
    payload: ReviewPayload;
    live: SessionRecord | null;
    replaceJobId?: string;
  }): "start" | "reuse" | "replace" | "conflict";
  buildConflict(
    live: SessionRecord,
    state: string,
  ): { code: string; error: string; existing: Record<string, unknown> };
  buildSwitchedResult(live: SessionRecord, focus: string): { summary: string; review: Record<string, unknown> };
};

interface ReviewPayload {
  project: string;
  repo: string;
  prId: unknown;
  sourceBranch: string;
  targetBranch?: string;
  title?: string;
  prUrl?: string;
}

const EDITOR_LABELS: Record<EditorId, string> = {
  vscode: "VS Code",
  cursor: "Cursor",
  "claude-code": "Claude Code",
};

export function createReviewInEditorFeature(config: Config): Feature {
  void restoreLiveSessions().catch(() => {});
  return {
    id: "review-in-editor",
    label: "Review PR",

    // Runs to completion inside /start, same shape as create-jira-subtasks
    // (no separate approve step — there's nothing to approve; the whole
    // point is that you review the checkout yourself, in your own
    // editor). Never rejects: any failure lands the job in "failed" so the
    // extension's one-shot flow (content.js's runOneShot) gets
    // job.error back through the normal 200-response path rather than an
    // HTTP error.
    //
    // The exception is a review still live in the repo's single review
    // worktree: the same PR switches to it (its job, terminal brought to
    // the front), and a different one is refused with a 409 unless the
    // request names that review as the one to cancel first, since checking
    // out would change the files under its agent. Starts are serialized
    // per worktree, so two quick clicks can't both check out.
    async start(payloadRaw: unknown): Promise<Job> {
      const payload = plan.assertPayload(payloadRaw);
      const replaceJobId = plan.parseReplaceJobId(payloadRaw);
      let repo: string;
      try {
        repo = resolveRepoPath(config, payload.project, payload.repo);
      } catch (err) {
        const job = jobStore.create("review-in-editor", { payload });
        return jobStore.update(job.id, { status: "failed", error: (err as Error).message });
      }
      const dir = reviewWorktree.reviewWorktreePath(repo);
      return withWorktreeLock(dir, async () => {
        const live = await reviewSession.findLiveSession(dir);
        const decision = plan.decideStart({ payload, live: live?.record ?? null, replaceJobId });
        if (live && decision === "reuse") return switchToLiveSession(live);
        if (live && decision === "conflict") {
          const conflict = plan.buildConflict(live.record, live.status.state);
          throw new StartRefusedError(conflict.error, conflict.code, { existing: conflict.existing });
        }
        const job = jobStore.create("review-in-editor", { payload });
        try {
          if (live && decision === "replace") await replaceLiveSession(live);
          const { summary, review } = await runReview(repo, dir, payload, config, job);
          return jobStore.update(job.id, { status: "approved", result: { summary, files: [], review } });
        } catch (err) {
          return jobStore.update(job.id, { status: "failed", error: (err as Error).message });
        }
      });
    },

    // A terminal-based review is tracked through its status file (see
    // core/review-session.js) until it stops; the final status is then
    // kept on the job and the file (and the worktree's record of it)
    // deleted.
    async sessionStatus(job: Job): Promise<SessionStatus | undefined> {
      const session = job.data.session as JobSession | undefined;
      if (!session) return undefined;
      if (session.final) return session.final;
      const status = await reviewSession.readStatus(session.file);
      if (!reviewSession.isLive(status)) {
        session.final = status;
        fs.rmSync(session.file, { force: true });
        reviewSession.removeSessionRecord(session.worktree, job.id);
      }
      return status;
    },

    async stopSession(job: Job): Promise<void> {
      const session = job.data.session as JobSession | undefined;
      if (!session || session.final) throw new Error("This review isn't running.");
      await reviewSession.stopSession(session.file);
    },

    async approve(job: Job): Promise<void> {
      throw new Error(`Cannot approve job in status "${job.status}" — review-in-editor has no approve step`);
    },

    async reject(job: Job): Promise<void> {
      throw new Error(`Cannot reject job in status "${job.status}" — review-in-editor has no reject step`);
    },
  };
}

interface JobSession {
  file: string;
  worktree: string;
  final?: SessionStatus;
}

const worktreeLocks = new Map<string, Promise<unknown>>();

/** Runs `fn` once every earlier call for the same worktree has settled. */
function withWorktreeLock<T>(dir: string, fn: () => Promise<T>): Promise<T> {
  const key = path.resolve(dir);
  const run = (worktreeLocks.get(key) || Promise.resolve()).then(fn);
  const settled = run.catch(() => undefined);
  worktreeLocks.set(key, settled);
  void settled.then(() => {
    if (worktreeLocks.get(key) === settled) worktreeLocks.delete(key);
  });
  return run;
}

/** The job for a live session record, recreated from the record if the
 * service has restarted since the review started. */
function jobForSession(record: SessionRecord): Job {
  const now = Date.now();
  const session: JobSession = { file: reviewSession.statusFileFor(record.jobId), worktree: record.worktree };
  return jobStore.restore({
    id: record.jobId,
    featureId: "review-in-editor",
    status: "approved",
    data: { payload: record.payload, session },
    result: { summary: record.summary, files: [], review: record.review },
    createdAt: now,
    updatedAt: now,
  });
}

/** Puts back the jobs of reviews still running when the service stopped,
 * so their panels (reopened after a reload) keep working; forgets the
 * rest. */
async function restoreLiveSessions(): Promise<void> {
  for (const record of reviewSession.listSessionRecords()) {
    const file = reviewSession.statusFileFor(record.jobId);
    if (reviewSession.isLive(await reviewSession.readStatus(file))) {
      jobForSession(record);
    } else {
      reviewSession.removeSessionRecord(record.worktree, record.jobId);
      fs.rmSync(file, { force: true });
    }
  }
}

/** /start's answer for the PR whose review is already live: that review's
 * job, with a result saying so, after bringing its terminal to the front.
 * The stored job is left as it was. */
async function switchToLiveSession(live: LiveSession): Promise<Job> {
  const job = jobForSession(live.record);
  let focus = "unknown";
  if (live.record.terminal) {
    try {
      focus = (await terminalFocus.focusTerminal(live.record.terminal)) ? "focused" : "missing";
    } catch (err) {
      focus = (err as Error).message;
    }
  }
  const { summary, review } = plan.buildSwitchedResult(live.record, focus);
  return { ...job, result: { summary, files: [], review } };
}

/** Cancels the live review the user chose to replace and waits for it to
 * stop; throws, before anything is checked out, if it doesn't. */
async function replaceLiveSession(live: LiveSession): Promise<void> {
  if (live.status.state !== "running") {
    throw new Error("The other review is still starting, so it can't be cancelled yet. Try again in a moment.");
  }
  const file = reviewSession.statusFileFor(live.record.jobId);
  let stopError: Error | undefined;
  try {
    await reviewSession.stopSession(file);
  } catch (err) {
    stopError = err as Error;
  }
  if (reviewSession.isLive(await reviewSession.waitUntilStopped(file))) {
    throw new Error(
      `${stopError ? `${stopError.message} ` : ""}The other review didn't stop, so this pull request wasn't ` +
        "checked out. Close its terminal window, then try again.",
    );
  }
}

async function runReview(
  repoPath: string,
  reviewDir: string,
  payload: ReviewPayload,
  config: Config,
  job: Job,
): Promise<{ summary: string; review: Record<string, unknown> }> {
  const { sourceBranch } = payload;
  let dir = reviewDir;
  if (!(await reviewWorktree.isRegisteredWorktree(repoPath, dir))) {
    // A repo added *after* setup (via config.ts's repoPath auto-inference)
    // never went through provision-review-worktrees.js — provision it now,
    // on demand, rather than failing.
    await reviewWorktree.provisionReviewWorktree(repoPath);
  }

  await git(["fetch", "origin", "--prune"], repoPath);

  // The fetch above pruned it if the PR was merged and its branch deleted;
  // say so rather than surface git's "not a commit" error from the checkout.
  const remoteRef = await git(["rev-parse", "--verify", "--quiet", `refs/remotes/origin/${sourceBranch}`], repoPath, {
    allowFailure: true,
  });
  if (remoteRef.code !== 0) {
    throw new Error(
      `The branch "${sourceBranch}" no longer exists on the remote, so there is nothing to check out. ` +
        "This usually means the pull request was merged (or declined) and its branch deleted.",
    );
  }

  // The branch is already checked out in another worktree of this repo (the
  // user's own working copy, say): `git checkout -B` would fail there, so
  // review in that checkout as it is rather than touching it.
  const worktreeList = (await git(["worktree", "list", "--porcelain"], repoPath)).stdout;
  const existing = findWorktreeOnBranch(worktreeList, sourceBranch, dir);
  let dirtyFiles: string[] = [];
  if (existing) {
    dir = existing;
  } else {
    const status = (await git(["status", "--porcelain"], dir)).stdout;
    dirtyFiles = plan.parseDirtyFiles(status);
    if (dirtyFiles.length > 0) {
      await git(["stash", "push", "-u", "-m", `review-in-editor ${sourceBranch}`], dir);
    }
    await git(["checkout", "-B", sourceBranch, "--track", `origin/${sourceBranch}`], dir);
  }

  const editor = resolveReviewEditor(config.reviewEditor);
  if (!editor) {
    throw new Error(
      "No editor configured for Review PR, and none could be auto-detected on this machine. " +
        "Run `npm run setup` again to pick one.",
    );
  }
  const useSkill = !!payload.prUrl && plan.hasReviewSkill(editor, dir);
  // Only the Claude Code terminal review can be tracked; the IDE chats
  // (Copilot Chat, Cursor's Agent window) can't.
  const statusFile = editor === "vscode" || editor === "cursor" ? undefined : reviewSession.createStatusFile(job.id);
  let launch;
  try {
    launch = await openForReview(
      dir,
      editor,
      plan.buildReviewPrompt({ ...payload, useSkill }),
      plan.buildTerminalHeader({ ...payload, editor, dir }),
      { ...plan.buildTerminalTitle(payload), statusFile },
    );
  } catch (err) {
    if (statusFile) fs.rmSync(statusFile, { force: true });
    throw err;
  }
  const { openedEditor, reviewStarted, reviewNote } = launch;
  const tracked = !!statusFile && reviewStarted;
  if (!tracked && statusFile) fs.rmSync(statusFile, { force: true });

  const summary = plan.buildSummary({
    branch: sourceBranch,
    dir,
    editorLabel: openedEditor ? EDITOR_LABELS[editor] : undefined,
    stashedFiles: dirtyFiles,
    reviewNote,
    usedSkill: reviewStarted && useSkill,
  });
  const review = plan.buildReviewResult({
    payload,
    dir,
    editor,
    reviewStarted,
    reviewNote,
    usedSkill: useSkill,
    stashedFiles: dirtyFiles,
    tracked,
  });
  if (tracked && statusFile) {
    const session: JobSession = { file: statusFile, worktree: dir };
    jobStore.update(job.id, { data: { ...job.data, session } });
    reviewSession.writeSessionRecord({ jobId: job.id, worktree: dir, payload, summary, review, terminal: launch.terminal });
  }
  return { summary, review };
}

/** The path of whatever *other* registered worktree of this repo already
 * has `branch` checked out, if any — `reviewDir` is excluded since landing
 * back on the review worktree's own current branch is not a conflict. */
function findWorktreeOnBranch(porcelainOutput: string, branch: string, reviewDir: string): string | null {
  for (const entry of porcelainOutput.split("\n\n")) {
    const lines = entry.split("\n");
    const pathLine = lines.find((l) => l.startsWith("worktree "));
    const branchLine = lines.find((l) => l.startsWith("branch "));
    if (!pathLine || !branchLine) continue;
    const wtPath = pathLine.slice("worktree ".length).trim();
    const wtBranch = branchLine.slice("branch ".length).trim().replace(/^refs\/heads\//, "");
    if (wtBranch === branch && path.resolve(wtPath) !== path.resolve(reviewDir)) {
      return wtPath;
    }
  }
  return null;
}
