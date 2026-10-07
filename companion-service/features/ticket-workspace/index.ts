// Ticket workspace: on a Jira ticket, everything this machine knows about
// it — the local history first (worktrees, PRs, analyses, Claude sessions
// linked to the key), then git (branches and worktrees whose name has the
// key: dirty, commits ahead), then Bitbucket (those branches' PRs, and the
// open PRs' build state). Read-only: it runs on a click, reads, and reports.
// Its actions only open a local worktree or resume a recorded session.
import * as fs from "fs";
import * as path from "path";
import type { Config } from "../../config";
import type { AuthContext } from "../../core/atlassian";
import { BuildSummary, GitHost, PullRequestSummary, providersFor } from "../../core/providers";
import { resolveReviewEditor, EditorId, openInEditor } from "../../core/editor";
import type { HistoryDetail, HistoryStore } from "../../core/history";
import {
  Feature,
  FeatureContext,
  FeatureDeps,
  Job,
  JobStatusError,
  McpToolContext,
  WorktreeGoneError,
  jobStore,
} from "../../core/jobs";
import { resumeSessionInTerminal } from "../../core/resume-session";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const ticketGit = require("../../core/ticket-git.js") as {
  ticketWorktreePath(repoPath: string, issueKey: string): string;
  scanRepoForTicket(
    repoPath: string,
    issueKey: string,
  ): Promise<{ branches: Array<{ name: string; remote: boolean }>; worktrees: unknown[] }>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const sessionCwd = require("../../core/session-cwd.js") as {
  sessionCwdAllowed(cwd: string, opts: { repoPaths: string[]; sessionsRoot: string }): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("../../core/paths.js") as { stateDir(): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { redactSecrets } = require("../../core/history-record.js") as { redactSecrets(text: string): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const prereqs = require("../../core/prereqs.js") as { isSafeRepoSegment(v: unknown): boolean };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  parseWorkspacePayload(payload: unknown): { issueKey: string };
  secondHopKeys(ticket: HistoryDetail): string[];
  sessionKeysOf(details: HistoryDetail[]): string[];
  historyFacts(
    ticket: HistoryDetail | null,
    related: Record<string, HistoryDetail>,
    policy: { worktreeOk(w: { repoKey: unknown; dir: string }): boolean; sessionCwdOk(cwd: string): boolean },
  ): unknown;
  buildWorkspace(input: {
    issueKey: string;
    bitbucketBaseUrl: string;
    repos: unknown[];
    history: unknown;
    notes: string[];
    exists: (dir: string) => boolean;
    cwdAllowed: (dir: string) => boolean;
  }): Workspace;
  workspaceSummary(ws: Workspace): string;
};

/** A pull request Jira's development panel lists for the ticket (analyze-issue's
 * plan.linkedPullRequests): its address is rebuilt on the configured host. */
export interface LinkedPr {
  repoKey: string;
  id: number;
  title: string | null;
  state: string | null;
  url: string | null;
}

const analyzePlan = require("../analyze-issue/plan.js") as {
  linkedPullRequests(issue: unknown, devStatus: unknown, bitbucketBaseUrl: string): LinkedPr[];
};

export interface Workspace {
  issueKey: string;
  worktrees: Array<{ repoKey: string; dir: string; branch: string | null; exists?: boolean; isTicketWorktree?: boolean }>;
  branches: Array<{ repoKey: string; name: string }>;
  prs: Array<{ repoKey: string; id: number; state: string | null; url: string }>;
  analyses: unknown[];
  sessions: Array<{ id: string; cwd: string; permissionMode: "plan" | "auto" | "default"; at: number }>;
  notes: string[];
  suggestions: { startFix: boolean; createPr: { repoKey: string; dir: string; branch: string } | null };
  resume: { session: Workspace["sessions"][number] | null; worktreeDir: string | null };
}

/** The I/O computeWorkspace does, injectable so a test can run it with no
 * network and no credentials (features/ticket-workspace/workspace.test.js). */
export interface WorkspaceIo {
  git(config: Config): GitHost;
  scanRepoForTicket(
    repoPath: string,
    issueKey: string,
  ): Promise<{ branches: Array<{ name: string; remote: boolean }>; worktrees: unknown[] }>;
  listBranchPullRequests(git: GitHost, auth: AuthContext, project: string, repo: string, branch: string): Promise<PullRequestSummary[]>;
  getCommitBuildStatus(git: GitHost, auth: AuthContext, project: string, repo: string, sha: string): Promise<BuildSummary | null>;
  /** The PRs linked to the ticket in Jira, whatever became of their branch. */
  linkedPullRequests?(config: Config, auth: AuthContext, issueKey: string): Promise<LinkedPr[]>;
}

/** Jira's development panel: the only place a PR whose branch was deleted
 * after the merge can still be found from the ticket. */
async function jiraLinkedPullRequests(config: Config, auth: AuthContext, issueKey: string): Promise<LinkedPr[]> {
  const { issues, git } = providersFor(config);
  const issue = (await issues.getIssueRaw(auth, issueKey, "summary")) as { id?: unknown };
  if (typeof issue.id !== "string" || !/^\d+$/.test(issue.id)) return [];
  const devStatus = await issues.getLinkedPullRequestsRaw(auth, issue.id);
  return analyzePlan.linkedPullRequests({}, devStatus, git.baseUrl());
}

export const REAL_WORKSPACE_IO: WorkspaceIo = {
  git: (config) => providersFor(config).git,
  scanRepoForTicket: (repoPath, issueKey) => ticketGit.scanRepoForTicket(repoPath, issueKey),
  listBranchPullRequests: (git, auth, project, repo, branch) => git.listBranchPullRequests(auth, project, repo, branch),
  getCommitBuildStatus: (git, auth, project, repo, sha) => git.getCommitBuildStatus(auth, project, repo, sha),
  linkedPullRequests: jiraLinkedPullRequests,
};

const MAX_REPOS = 30;
const MAX_PR_BRANCHES = 10;
const MAX_BUILD_LOOKUPS = 5;

/** The folders this service may open a session or a terminal in: the same
 * allow-list a persisted job file gets (server.ts's sessionCwdOk). */
function cwdAllowedFor(config: Config): (cwd: string) => boolean {
  return (cwd) =>
    sessionCwd.sessionCwdAllowed(cwd, {
      repoPaths: Object.values(config.repos || {}),
      sessionsRoot: path.join(paths.stateDir(), "sessions"),
    });
}

/** A history row's worktree is only believed if it is exactly the ticket
 * worktree of a configured repository. */
function historyWorktreeOk(config: Config, issueKey: string, w: { repoKey: unknown; dir: string }): boolean {
  if (typeof w.repoKey !== "string" || !Object.prototype.hasOwnProperty.call(config.repos || {}, w.repoKey)) return false;
  const repoPath = config.repos[w.repoKey];
  return typeof repoPath === "string" && path.resolve(w.dir) === path.resolve(ticketGit.ticketWorktreePath(repoPath, issueKey));
}

function historyView(config: Config, history: HistoryStore | undefined, issueKey: string): unknown {
  if (!history) return null;
  const ticket = history.getItem(`jira:${issueKey}`);
  if (!ticket) return null;
  const related: Record<string, HistoryDetail> = {};
  for (const key of plan.secondHopKeys(ticket)) {
    const d = history.getItem(key);
    if (d) related[key] = d;
  }
  for (const key of plan.sessionKeysOf(Object.values(related))) {
    const d = history.getItem(key);
    if (d) related[key] = d;
  }
  return plan.historyFacts(ticket, related, {
    worktreeOk: (w) => historyWorktreeOk(config, issueKey, w),
    sessionCwdOk: cwdAllowedFor(config),
  });
}

/**
 * The workspace for `issueKey`. Never throws for one source failing: a repo
 * git can't read, or a Bitbucket call that fails, becomes a line in `notes`.
 * Shared by the ✨ panel (start) and the MCP tool / `companion resume`.
 */
export async function computeWorkspace(
  config: Config,
  deps: FeatureDeps,
  auth: AuthContext,
  issueKeyRaw: string,
  onProgress: (stepId: string, label: string) => void = () => {},
  io: WorkspaceIo = REAL_WORKSPACE_IO,
): Promise<Workspace> {
  // Every caller (panel, MCP tool, CLI) comes through here: check the key first.
  const { issueKey } = plan.parseWorkspacePayload({ issueKey: issueKeyRaw });
  const notes: string[] = [];
  onProgress("history", "Reading the local history…");
  let history: unknown = null;
  try {
    history = historyView(config, deps.history, issueKey);
  } catch (err) {
    notes.push(`Local history: ${(err as Error).message}`);
  }

  onProgress("git", "Looking for branches and worktrees…");
  const repos: Array<{ repoKey: string; branches: Array<{ name: string; remote: boolean }>; worktrees: unknown[]; prs: unknown[] }> = [];
  for (const [repoKey, repoPath] of Object.entries(config.repos || {}).slice(0, MAX_REPOS)) {
    if (!fs.existsSync(path.join(repoPath, ".git"))) continue;
    try {
      const scan = await io.scanRepoForTicket(repoPath, issueKey);
      if (scan.branches.length > 0 || scan.worktrees.length > 0) repos.push({ repoKey, ...scan, prs: [] });
    } catch (err) {
      notes.push(`${repoKey}: ${(err as Error).message}`);
    }
  }

  onProgress("git", "Checking for pull requests…");
  let gitOrNull: GitHost | null = null;
  try {
    gitOrNull = io.git(config);
  } catch (err) {
    notes.push(`Bitbucket: ${redactSecrets((err as Error).message)}`);
  }
  const git = gitOrNull;
  let prLookups = 0;
  let buildLookups = 0;
  for (const r of repos) {
    if (!git) break;
    const [project, repo] = r.repoKey.split("/");
    if (!prereqs.isSafeRepoSegment(project) || !prereqs.isSafeRepoSegment(repo)) continue;
    for (const b of r.branches.filter((x) => x.remote)) {
      if (prLookups >= MAX_PR_BRANCHES) break;
      prLookups += 1;
      let prs: Array<PullRequestSummary & { build?: BuildSummary | null }>;
      try {
        prs = await io.listBranchPullRequests(git, auth, project, repo, b.name);
      } catch (err) {
        notes.push(`Bitbucket (${r.repoKey} ${b.name}): ${(err as Error).message}`);
        continue;
      }
      for (const pr of prs) {
        if (pr.state === "OPEN" && pr.fromSha && buildLookups < MAX_BUILD_LOOKUPS) {
          buildLookups += 1;
          try {
            pr.build = await io.getCommitBuildStatus(git, auth, project, repo, pr.fromSha);
          } catch (err) {
            notes.push(`Build status (${r.repoKey} #${pr.id}): ${(err as Error).message}`);
          }
        }
        r.prs.push(pr);
      }
    }
  }

  // PRs Jira links to the ticket that the branch lookup can't reach — a merged
  // PR's branch is usually deleted. Best effort, like every other source here.
  if (io.linkedPullRequests) {
    try {
      for (const lp of await io.linkedPullRequests(config, auth, issueKey)) {
        const sameRepo = (x: { repoKey: string }) => x.repoKey.toLowerCase() === lp.repoKey.toLowerCase();
        if (repos.some((r) => sameRepo(r) && (r.prs as Array<{ id: number }>).some((p) => p.id === lp.id))) continue;
        let repo = repos.find(sameRepo);
        if (!repo) {
          repo = { repoKey: lp.repoKey, branches: [], worktrees: [], prs: [] };
          repos.push(repo);
        }
        repo.prs.push({ id: lp.id, title: lp.title, state: lp.state, url: lp.url, fromBranch: null, toBranch: null, build: null });
      }
    } catch (err) {
      notes.push(`Jira linked pull requests: ${redactSecrets((err as Error).message)}`);
    }
  }

  return plan.buildWorkspace({
    issueKey,
    bitbucketBaseUrl: git ? git.baseUrl() : "",
    repos,
    history,
    notes,
    exists: (dir) => fs.existsSync(dir),
    cwdAllowed: cwdAllowedFor(config),
  });
}

function editorFor(config: Config): EditorId {
  return resolveReviewEditor(config.reviewEditor) || "claude-code";
}

function workspaceOf(job: Job): Workspace {
  if (job.status !== "awaiting-approval") throw new JobStatusError("Scan the ticket again first — this workspace is closed.");
  const ws = (job.data as { workspace?: Workspace }).workspace;
  if (!ws) throw new JobStatusError("This scan has no workspace yet.");
  return ws;
}

export function createTicketWorkspaceFeature(
  config: Config,
  deps: FeatureDeps = { providers: providersFor(config) },
  io: WorkspaceIo = REAL_WORKSPACE_IO,
): Feature {
  return {
    id: "ticket-workspace",
    label: "Ticket workspace",

    // Polled read-only shape (like analyze-issue): /start returns the
    // running job; it lands in awaiting-approval with data.workspace.
    async start(payloadRaw: unknown, ctx: FeatureContext): Promise<Job> {
      const { issueKey } = plan.parseWorkspacePayload(payloadRaw);
      const job = jobStore.create("ticket-workspace", { issueKey });
      void computeWorkspace(
        config,
        deps,
        ctx.auth,
        issueKey,
        (stepId, label) => jobStore.setProgressIfRunning(job.id, stepId, label),
        io,
      )
        .then((workspace) => {
          jobStore.update(job.id, {
            status: "awaiting-approval",
            progress: undefined,
            data: { issueKey, workspace },
            result: { summary: plan.workspaceSummary(workspace), files: [] },
          });
        })
        .catch((err: Error) => jobStore.update(job.id, { status: "failed", progress: undefined, error: err?.message || String(err) }));
      return job;
    },

    async approve(job: Job): Promise<void> {
      throw new JobStatusError(`Cannot approve job in status "${job.status}" — the ticket workspace is a read-only report`);
    },

    async reject(job: Job): Promise<void> {
      if (job.status === "awaiting-approval" || job.status === "failed") {
        jobStore.update(job.id, { status: "rejected", progress: undefined });
        return;
      }
      throw new JobStatusError(`Cannot reject job in status "${job.status}"`);
    },

    actions: {
      // Body { dir }: must be one of this scan's own worktrees.
      async "open-worktree"(job: Job, ctx: FeatureContext): Promise<unknown> {
        const dir = (ctx.body as { dir?: unknown } | undefined)?.dir;
        const wt = workspaceOf(job).worktrees.find((w) => w.dir === dir);
        if (!wt) throw new JobStatusError("That worktree isn't part of this workspace.");
        if (!fs.existsSync(wt.dir)) throw new WorktreeGoneError(`${wt.dir} no longer exists.`);
        const editor = editorFor(config);
        await openInEditor(wt.dir, editor);
        return { ok: true, editor };
      },
      // Body { sessionId }: must be one of this scan's own sessions.
      async "resume-session"(job: Job, ctx: FeatureContext): Promise<unknown> {
        const id = (ctx.body as { sessionId?: unknown } | undefined)?.sessionId;
        const ws = workspaceOf(job);
        const session = ws.sessions.find((s) => s.id === id);
        if (!session) throw new JobStatusError("That session isn't part of this workspace.");
        const outcome = await resumeSessionInTerminal(config, session, [
          [{ text: `Ticket workspace  ${ws.issueKey}`, style: "title" }],
          [{ text: "Resuming Claude Code…", style: "status" }],
          [],
        ]);
        if (!outcome.ok) throw new WorktreeGoneError("That Claude Code session has expired (its folder or transcript is gone).");
        return { ok: true };
      },
    },

    // Read-only: computes the same view without a job.
    mcpTools() {
      return [
        {
          name: "ticket_workspace",
          async handler(args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
            return computeWorkspace(config, deps, ctx.auth, args.issueKey as string, undefined, io);
          },
        },
      ];
    },
  };
}
