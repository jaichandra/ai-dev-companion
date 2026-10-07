// The companion's MCP server, served statelessly at POST /mcp (server.ts
// mounts it). Its own trust boundary, separate from every other route:
//
//   - A bearer token of its own (`mcp.token` in credentials.enc, rotatable
//     from Settings), accepted ONLY here — and the extension's
//     X-Companion-Secret is never accepted here, not even compared. So a
//     leaked ~/.claude.json (where Claude Code keeps this token) can call
//     these tools but can't drive any of the extension's routes.
//   - A Host check (core/mcp-auth.js's hostAllowed) against DNS rebinding.
//   - No MCP tool pushes, posts, comments or writes config: they read, run
//     the read-only analyze-issue / pre-deployment-stats jobs, or only
//     *queue* a write-capable job as "pending-start", which runs only once
//     the user clicks Start on the PR's page in Chrome; the one write,
//     `forget_item`, only deletes the companion's own local history, and
//     `open_location` only opens a tracked file in a local editor.
//
// Uses the SDK's low-level Server (marked @deprecated in favor of the
// zod-based McpServer) on purpose: the tool schemas are plain JSON Schema
// from core/mcp-tools.js, and zod isn't a dependency. Stateless — a fresh
// Server + transport per request, no sessions, no SSE stream.
import * as fs from "fs";
import * as path from "path";
import type { NextFunction, Request, Response } from "express";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import type { Config } from "../config";
import type { HistoryStore } from "./history";
import type { AuthContext } from "./atlassian";
import { Feature, FeatureContext, Job, McpToolContext, McpToolDef, jobStore } from "./jobs";
import { providersFor } from "./providers";
import { openLocation } from "./open-location";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const addressReviewCommentsPlan = require("../features/address-review-comments/plan.js") as {
  collectOpenComments(
    comments: unknown[],
    me: string | null,
  ): Array<{
    id: number;
    authorSlug: string | null;
    severity?: string | null;
    state?: string | null;
    anchor?: { path?: string | null; line?: number | null } | null;
    text?: string | null;
  }>;
};

/** One core/mcp-tools.js TOOL_CATALOG entry, as far as this file reads it. */
interface CatalogDef {
  name: string;
  featureId: string | null;
  description: string;
  generic: boolean;
  params: Record<string, unknown>;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const mcpTools = require("./mcp-tools.js") as {
  TOOL_CATALOG: CatalogDef[];
  inputSchemaFor(def: CatalogDef): Record<string, unknown>;
  validateToolArgs(
    def: CatalogDef,
    args: unknown,
  ): { ok: true; value: Record<string, unknown> } | { ok: false; error: string };
  selectTools(opts: { enabledFeatureIds: string[]; externalServers: string[] }): CatalogDef[];
  capToolOutput(value: unknown, maxChars?: number): string;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const mcpAuth = require("./mcp-auth.js") as {
  bearerMatches(authorizationHeader: string | undefined, expectedToken: string | undefined): boolean;
  hostAllowed(hostHeader: string | undefined, port: number): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { COMPANION_MCP_SERVER } = require("./mcp-tool-classifier.js") as { COMPANION_MCP_SERVER: string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const scopeKey = require("./scope-key.js") as {
  scopeKeyFor(featureId: string, payload: unknown): string | null;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const prereqs = require("./prereqs.js") as {
  inferRepoPath(existingRepos: Record<string, string>, project: string, repo: string, extraRoots?: string[]): string | null;
  defaultSearchRoots(): string[];
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const backgroundTools = require("./mcp-background-tools.js") as {
  listNotifications(inbox: McpDeps["notifications"], args: Record<string, unknown>): unknown;
  assessPushRisk(
    args: Record<string, unknown>,
    deps: { riskUrl: string | undefined; riskFacts: { get(url: string): Promise<unknown> } },
  ): Promise<unknown>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const memoryTools = require("./mcp-memory-tools.js") as {
  findSimilarTool(find: McpDeps["findSimilar"], args: Record<string, unknown>): Promise<unknown>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const riskFactsModule = require("./risk-facts.js") as { shared: { get(url: string): Promise<unknown> } };

const LIST_JOBS_LIMIT = 50;
const DIFF_MAX_CHARS = 20_000;
const AWAIT_POLL_MS = 500;
const JIRA_ISSUE_FIELDS =
  "summary,status,issuetype,priority,assignee,reporter,components,labels,fixVersions,versions,description,environment,comment";
const JIRA_DESCRIPTION_MAX_CHARS = 20_000;
const JIRA_COMMENT_LIMIT = 20;
const JIRA_COMMENT_BODY_MAX_CHARS = 4_000;
const BITBUCKET_COMMENT_TEXT_MAX_CHARS = 4_000;

export interface McpDeps {
  config: Config;
  features: Feature[];
  version: string;
  port: number;
  getToken: () => string | undefined;
  history?: HistoryStore; // undefined when local history is off
  startFeatureJob: (
    feature: Feature,
    payload: unknown,
    ctx: FeatureContext,
    via: "extension" | "mcp",
  ) => Promise<Job>;
  /** Names of the user's own configured MCP servers (analyze-issue's readMcpServers). */
  readMcpServers: () => string[];
  /** Similar past tickets (core/similar-search.js); undefined when the history is off. */
  findSimilar?: import("./jobs").FindSimilar;
  /** The inbox (core/notifications.js); list_notifications reads it. */
  notifications?: { list(opts: { includeSeen?: boolean; limit?: number }): unknown[]; unseenCount(): number };
}

type Handler = McpToolDef["handler"];

function listJobs(args: Record<string, unknown>): unknown {
  return jobStore
    .list()
    .filter((job) => args.featureId === undefined || job.featureId === args.featureId)
    .filter((job) => args.scopeKey === undefined || job.scopeKey === args.scopeKey)
    .slice(0, LIST_JOBS_LIMIT)
    .map((job) => ({
      id: job.id,
      featureId: job.featureId,
      status: job.status,
      scopeKey: job.scopeKey,
      startedVia: job.startedVia,
      createdAt: job.createdAt,
      updatedAt: job.updatedAt,
      summary: job.result?.summary,
      error: job.error,
      progress: job.progress?.label,
    }));
}

function getJob(args: Record<string, unknown>): unknown {
  const job = jobStore.get(args.jobId as string);
  if (!job) throw new Error("No job with that id.");
  if (!job.result) return job;
  // A copy — never trim the stored job itself, which the extension still
  // renders in full.
  const files = job.result.files.map((file) =>
    file.diff.length > DIFF_MAX_CHARS
      ? {
          ...file,
          diff:
            `${file.diff.slice(0, DIFF_MAX_CHARS)}\n… (truncated: ${file.diff.length - DIFF_MAX_CHARS} more ` +
            "characters — open the job in Chrome for the full diff)",
        }
      : file,
  );
  return { ...job, result: { ...job.result, files } };
}

/**
 * `lookup_local_repo`: a read-only lookup, NOT config.ts's repoPath() —
 * which writes config.json (it saves an inferred mapping, and drops a
 * stale one). Review point: no MCP tool writes config. prereqs.inferRepoPath
 * only reads (existsSync plus `git remote get-url` / `git rev-parse`
 * through checkRepoPath and getOriginUrl), so it's safe to call here.
 */
function lookupLocalRepo(config: Config, args: Record<string, unknown>): unknown {
  const project = args.project as string;
  const repo = args.repo as string;
  const key = `${project}/${repo}`.toLowerCase();
  const repos = config.repos || {};
  const existingKey = Object.keys(repos).find((k) => k.toLowerCase() === key);
  if (existingKey && fs.existsSync(path.join(repos[existingKey], ".git"))) {
    return { path: repos[existingKey], source: "configured" };
  }
  const inferred = prereqs.inferRepoPath(repos, project, repo, prereqs.defaultSearchRoots());
  if (inferred) {
    return { path: inferred, source: "inferred", note: "Not saved to config — the first job for this repo will save it." };
  }
  return { path: null, source: "none" };
}

interface JiraUserRef {
  displayName?: string;
  name?: string;
}

/** A user reference's display name, falling back to the username — the
 * same precedence features/analyze-issue/plan.js's buildAnalysisPrompt
 * uses for comment authors. */
function jiraUserName(user: JiraUserRef | null | undefined): string | null {
  return user?.displayName || user?.name || null;
}

interface JiraIssueRaw {
  key?: string;
  fields?: {
    summary?: string;
    status?: { name?: string };
    issuetype?: { name?: string };
    priority?: { name?: string };
    assignee?: JiraUserRef | null;
    reporter?: JiraUserRef | null;
    components?: Array<{ name?: string }>;
    labels?: string[];
    fixVersions?: Array<{ name?: string }>;
    description?: string | null;
    comment?: { comments?: Array<{ author?: JiraUserRef; created?: string; body?: string }> };
  };
}

/** `jira_get_issue`: a compact read of one issue — this Jira Server
 * instance's description/comment bodies are wiki-markup strings (API v2),
 * so they're capped by length rather than parsed. */
async function jiraGetIssue(config: Config, args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
  const key = args.issueKey as string;
  const issue = (await providersFor(config).issues.getIssueRaw(ctx.auth, key, JIRA_ISSUE_FIELDS)) as JiraIssueRaw;
  const fields = issue.fields || {};
  const comments = (fields.comment?.comments || []).slice(-JIRA_COMMENT_LIMIT).map((c) => ({
    author: jiraUserName(c.author),
    created: c.created ?? null,
    body: typeof c.body === "string" ? c.body.slice(0, JIRA_COMMENT_BODY_MAX_CHARS) : "",
  }));
  return {
    key: issue.key || key,
    summary: fields.summary ?? null,
    status: fields.status?.name ?? null,
    type: fields.issuetype?.name ?? null,
    priority: fields.priority?.name ?? null,
    assignee: jiraUserName(fields.assignee),
    reporter: jiraUserName(fields.reporter),
    components: (fields.components || []).map((c) => c.name).filter(Boolean),
    labels: fields.labels || [],
    fixVersions: (fields.fixVersions || []).map((v) => v.name).filter(Boolean),
    description: typeof fields.description === "string" ? fields.description.slice(0, JIRA_DESCRIPTION_MAX_CHARS) : null,
    comments,
  };
}

/** `bitbucket_get_pull_request`: the same normalized shape
 * core/bitbucket.ts's getPullRequest already produces for every other
 * caller — nothing further to shape here. */
function bitbucketGetPullRequest(config: Config, args: Record<string, unknown>, ctx: McpToolContext): Promise<unknown> {
  return providersFor(config).git.getPullRequest(ctx.auth, args.project as string, args.repo as string, args.prId as number);
}

/** `bitbucket_list_open_comments`: features/address-review-comments'
 * own collectOpenComments (same "still open" rules the Address Review
 * Comments job uses), reduced to the fields worth handing to a model. `me`
 * is best-effort — a whoAmI failure degrades to "unknown user" rather than
 * failing the whole read (collectOpenComments already treats a null `me`
 * that way, skipping the self-authored-comment drop). */
async function bitbucketListOpenComments(
  config: Config,
  args: Record<string, unknown>,
  ctx: McpToolContext,
): Promise<unknown> {
  const { git } = providersFor(config);
  const project = args.project as string;
  const repo = args.repo as string;
  const prId = args.prId as number;
  const me = await git.whoAmI(ctx.auth).catch(() => null);
  const activities = await git.listActivities(ctx.auth, project, repo, prId);
  const open = addressReviewCommentsPlan.collectOpenComments(activities, me);
  return open.map((c) => ({
    id: c.id,
    author: c.authorSlug ?? null,
    severity: c.severity ?? null,
    state: c.state ?? null,
    path: c.anchor?.path ?? null,
    line: typeof c.anchor?.line === "number" ? c.anchor.line : null,
    text: typeof c.text === "string" ? c.text.slice(0, BITBUCKET_COMMENT_TEXT_MAX_CHARS) : "",
  }));
}

/** Resolves with the job once it leaves "running" or `timeoutMs` passes —
 * and, should it vanish from the store mid-poll, with the last snapshot
 * seen (the callers already handle a still-"running" job by handing back
 * its jobId). Only a job that never existed at all rejects. */
async function awaitJob(jobId: string, timeoutMs: number): Promise<Job> {
  const deadline = Date.now() + timeoutMs;
  let last = jobStore.get(jobId);
  if (!last) throw new Error("No job with that id.");
  for (;;) {
    const job = jobStore.get(jobId);
    if (!job) return last;
    last = job;
    if (job.status !== "running" || Date.now() >= deadline) return job;
    await new Promise((resolve) => setTimeout(resolve, AWAIT_POLL_MS));
  }
}

/** The PR check both queueing tools share (start_resolve_conflict,
 * start_address_review_comments): the PR must exist, be open and have
 * both branches — checked now, so Claude hears about a bad PR number
 * straight away instead of the user finding a pending job that can't
 * start. */
export async function fetchOpenPullRequest(
  config: Config,
  auth: AuthContext,
  project: string,
  repo: string,
  prId: number,
): Promise<{ fromBranch: string; toBranch: string }> {
  const pr = (await providersFor(config).git.getPullRequest(auth, project, repo, prId)) as {
    state?: string | null;
    fromBranch?: string | null;
    toBranch?: string | null;
  } | null;
  if (!pr) throw new Error(`Bitbucket didn't return pull request ${project}/${repo} #${prId}.`);
  if (pr.state !== "OPEN") {
    throw new Error(`Pull request ${project}/${repo} #${prId} is ${(pr.state || "not open").toLowerCase()}, not open.`);
  }
  if (!pr.fromBranch || !pr.toBranch) {
    throw new Error(`Bitbucket didn't report the source and target branches of ${project}/${repo} #${prId}.`);
  }
  return { fromBranch: pr.fromBranch, toBranch: pr.toBranch };
}

/** What a queueing tool returns once its "pending-start" job exists. */
export function pendingStartResult(config: Config, job: Job, project: string, repo: string, prId: number): unknown {
  return {
    jobId: job.id,
    status: "pending-start",
    prUrl: providersFor(config).git.prUrl(project, repo, prId),
    message: "Queued. Open the PR in Chrome and click Start in the ✨ panel — nothing runs until then.",
  };
}

function errorResult(text: string) {
  return { isError: true, content: [{ type: "text" as const, text }] };
}

const JSON_RPC_INTERNAL_ERROR = { jsonrpc: "2.0", error: { code: -32603, message: "Internal error" }, id: null };
const JSON_RPC_PARSE_ERROR = { jsonrpc: "2.0", error: { code: -32700, message: "Parse error" }, id: null };

/** Only ever the error's message — never a request's arguments, and
 * nothing here ever holds the token. */
function logFailure(where: string, err: unknown): void {
  console.error(`[mcp] ${where} failed: ${(err as Error)?.message || String(err)}`);
}

/**
 * The /mcp access check, shared by `gate` (mounted before the JSON
 * parser, so nothing unauthenticated is ever parsed) and by `post` (a
 * second check, in case the gate is ever mounted wrong): the Host check,
 * then the token's existence, then the bearer. X-Companion-Secret is
 * deliberately never looked at. Returns the refusal to send, or null to
 * let the request through. May throw (a credential-store read error) —
 * both callers catch.
 */
function accessRefusal(
  req: Request,
  deps: Pick<McpDeps, "port" | "getToken">,
): { status: number; body: Record<string, unknown>; bearerChallenge?: boolean } | null {
  const { port } = deps;
  if (!mcpAuth.hostAllowed(req.header("host"), port)) {
    return {
      status: 403,
      body: { error: `/mcp only answers requests addressed to 127.0.0.1:${port} or localhost:${port}.` },
    };
  }
  const token = deps.getToken();
  if (!token) {
    return {
      status: 503,
      body: {
        error:
          "The companion's MCP token isn't set up yet — run the installer again (node install.js --yes) or npm run setup.",
      },
    };
  }
  if (!mcpAuth.bearerMatches(req.header("authorization"), token)) {
    return { status: 401, body: { error: "Missing or wrong MCP token." }, bearerChallenge: true };
  }
  return null;
}

/** Sends accessRefusal's result; true if the request was refused. */
function refuseIfNotAllowed(req: Request, res: Response, deps: Pick<McpDeps, "port" | "getToken">): boolean {
  const refusal = accessRefusal(req, deps);
  if (!refusal) return false;
  if (refusal.bearerChallenge) res.set("WWW-Authenticate", "Bearer");
  res.status(refusal.status).json(refusal.body);
  return true;
}

function requireHistory(deps: McpDeps): HistoryStore {
  if (!deps.history) {
    throw new Error("Local history is off on this copy of the companion (it only records on the installed copy).");
  }
  return deps.history;
}

export function createMcpHandlers(deps: McpDeps): {
  gate(req: Request, res: Response, next: NextFunction): void;
  bodyErrors(err: unknown, req: Request, res: Response, next: NextFunction): void;
  post(req: Request, res: Response): Promise<void>;
  notAllowed(req: Request, res: Response): void;
} {
  const { config, features, version } = deps;
  const catalogByName = new Map(mcpTools.TOOL_CATALOG.map((def) => [def.name, def]));

  const handlers = new Map<string, Handler>([
    ["list_jobs", async (args) => listJobs(args)],
    ["get_job", async (args) => getJob(args)],
    ["lookup_local_repo", async (args) => lookupLocalRepo(config, args)],
    [
      "get_history",
      async (args) => {
        const detail = requireHistory(deps).getItem(args.key as string);
        if (!detail) throw new Error("Nothing is recorded under that key.");
        return detail;
      },
    ],
    [
      "list_history",
      async (args) => {
        const history = requireHistory(deps);
        const days = (args.days as number | undefined) ?? 30;
        return {
          days,
          metrics: history.metrics({ days }),
          prewarmed: history.prewarmMetrics({ days }),
          matches: typeof args.query === "string" ? history.search(args.query, 20) : [],
        };
      },
    ],
    ["find_similar", async (args) => memoryTools.findSimilarTool(deps.findSimilar, args)],
    ["forget_item", async (args) => requireHistory(deps).forget(args.key as string)],
    ["open_location", async (args) => openLocation(config, { text: args.text as string })],
    ["list_notifications", async (args) => backgroundTools.listNotifications(deps.notifications, args)],
    [
      "get_change_risk",
      async (args) => backgroundTools.assessPushRisk(args, { riskUrl: config.riskFacts?.url, riskFacts: riskFactsModule.shared }),
    ],
    ["jira_get_issue", async (args, toolCtx) => jiraGetIssue(config, args, toolCtx)],
    ["bitbucket_get_pull_request", async (args, toolCtx) => bitbucketGetPullRequest(config, args, toolCtx)],
    ["bitbucket_list_open_comments", async (args, toolCtx) => bitbucketListOpenComments(config, args, toolCtx)],
  ]);
  // selectedTools() below only advertises a selected tool when it also has
  // a handler here — belt-and-suspenders against the catalog and this map
  // drifting apart (e.g. a future catalog entry added without its
  // handler), not because any tool is missing one today.
  for (const feature of features) {
    for (const tool of feature.mcpTools?.() ?? []) {
      const def = catalogByName.get(tool.name);
      // A programming error, not a runtime condition: a feature may only
      // serve catalog tools that belong to it.
      if (!def || def.featureId !== feature.id) {
        throw new Error(`Feature ${feature.id} offers MCP tool "${tool.name}", which the catalog doesn't give it.`);
      }
      handlers.set(tool.name, tool.handler);
    }
  }

  const ctx: McpToolContext = {
    // Empty on purpose: every authedJson call resolves auth through
    // core/auth-context.ts (vault, then saved token) — an MCP call has no
    // browser cookie of its own.
    auth: {},
    startJob: (featureId, payload) => {
      const feature = features.find((f) => f.id === featureId);
      if (!feature) throw new Error(`Feature "${featureId}" isn't enabled.`);
      return deps.startFeatureJob(feature, payload, { auth: {}, body: payload }, "mcp");
    },
    createPendingJob: (featureId, payload) => {
      const key = scopeKey.scopeKeyFor(featureId, payload);
      if (!key) throw new Error(`Can't queue a ${featureId} job for that payload.`);
      return jobStore.createPending(featureId, payload, key);
    },
    awaitJob,
  };

  async function selectedTools(): Promise<CatalogDef[]> {
    let externalServers: string[] = [];
    try {
      externalServers = deps.readMcpServers();
    } catch {
      // Counts as "none configured" — the worst case is offering the
      // generic readers when they'd have been redundant.
    }
    return mcpTools
      .selectTools({
        enabledFeatureIds: features.map((f) => f.id),
        externalServers,
      })
      .filter((def) => handlers.has(def.name));
  }

  /** Mounted on /mcp BEFORE the JSON parser: nothing unauthenticated is
   * ever parsed (a 5 MB body without the token is a 401, not a 413).
   * Only POST is gated — GET/DELETE fall through to notAllowed's 405. */
  function gate(req: Request, res: Response, next: NextFunction): void {
    if (req.method !== "POST") {
      next();
      return;
    }
    try {
      if (refuseIfNotAllowed(req, res, deps)) return;
    } catch (err) {
      logFailure("access check", err);
      if (!res.headersSent) res.status(500).json({ error: "The companion couldn't check the MCP token." });
      return;
    }
    next();
  }

  /** Mounted on /mcp right after the JSON parser: a body the parser
   * rejects gets a JSON-RPC error rather than Express's HTML error page
   * (which shows a stack trace). */
  function bodyErrors(err: unknown, _req: Request, res: Response, next: NextFunction): void {
    if (res.headersSent) {
      next(err);
      return;
    }
    const type = (err as { type?: unknown } | null)?.type;
    if (type === "entity.parse.failed") {
      res.status(400).json(JSON_RPC_PARSE_ERROR);
      return;
    }
    if (type === "entity.too.large") {
      res.status(413).json({ jsonrpc: "2.0", error: { code: -32600, message: "Request too large" }, id: null });
      return;
    }
    logFailure("request body", err);
    res.status(500).json(JSON_RPC_INTERNAL_ERROR);
  }

  async function post(req: Request, res: Response): Promise<void> {
    // The whole body is caught: this runs from an Express route with no
    // one awaiting it, and an unhandled rejection would take the service
    // down (e.g. a credential-store read error from getToken).
    try {
      // Already checked by `gate`; repeated so a mounting mistake can
      // never leave this open.
      if (refuseIfNotAllowed(req, res, deps)) return;

      const tools = await selectedTools();
      const server = new Server({ name: COMPANION_MCP_SERVER, version }, { capabilities: { tools: {} } });
      server.setRequestHandler(ListToolsRequestSchema, async () => ({
        tools: tools.map((d) => ({ name: d.name, description: d.description, inputSchema: mcpTools.inputSchemaFor(d) })),
      }));
      server.setRequestHandler(CallToolRequestSchema, async (request) => {
        const { name, arguments: args } = request.params;
        const def = tools.find((d) => d.name === name);
        // Only a selected tool's own name ever reaches the log — a
        // client-chosen string could be anything.
        if (!def) return errorResult(`Unknown tool ${name}`);
        const started = Date.now();
        const finish = (outcome: "ok" | "error") =>
          console.log(`[mcp] tools/call ${def.name} ${outcome} (${Date.now() - started} ms)`);
        const checked = mcpTools.validateToolArgs(def, args ?? {});
        if (!checked.ok) {
          finish("error");
          return errorResult(checked.error);
        }
        try {
          const result = await handlers.get(def.name)!(checked.value, ctx);
          finish("ok");
          return { content: [{ type: "text" as const, text: mcpTools.capToolOutput(result) }] };
        } catch (err) {
          finish("error");
          // AuthSetupError's messages are already written for the user.
          return errorResult((err as Error)?.message || String(err));
        }
      });

      const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
      res.on("close", () => {
        void transport.close().catch(() => {});
        void server.close().catch(() => {});
      });
      await server.connect(transport);
      await transport.handleRequest(req, res, req.body);
    } catch (err) {
      logFailure("request", err);
      if (!res.headersSent) res.status(500).json(JSON_RPC_INTERNAL_ERROR);
    }
  }

  function notAllowed(_req: Request, res: Response): void {
    // Stateless: no SSE stream to GET, no session to DELETE.
    res.status(405).set("Allow", "POST").json({ error: "Only POST is supported on /mcp." });
  }

  return { gate, bodyErrors, post, notAllowed };
}
