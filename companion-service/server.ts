import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { spawnSync } from "child_process";
import express, { NextFunction, Request, Response } from "express";
import {
  Config,
  ConfigMissingError,
  DEFAULT_BITBUCKET_BASE_URL,
  DEFAULT_JENKINS_BASE_URL,
  DEFAULT_JIRA_BASE_URL,
  RepoNotFoundError,
  loadConfig,
  readConfigFile,
  repoPath,
  writeConfig,
} from "./config";
import { RepoSetupError, chooseRepoFolder, startClone } from "./core/repo-setup";
import {
  jobStore,
  Feature,
  FeatureDeps,
  FeatureContext,
  FindSimilar,
  Job,
  StartRefusedError,
  JobStatusError,
  WorktreeGoneError,
} from "./core/jobs";
import { AuthSetupError } from "./core/atlassian";
import type { AuthContext } from "./core/atlassian";
import { providersFor } from "./core/providers";
import { HistoryStore, openHistoryStore } from "./core/history";
import { configureSessionVault, configuredOrigins, heartbeatHosts, recordRelayedSession } from "./core/auth-context";
import { detachedChildCount, killDetachedChildren, run } from "./core/exec";
import { WORKTREE_ROOT, removeWorktree } from "./core/worktree";
import type { WorktreeHandle } from "./core/worktree";
import { openClaudeCodeInTerminal } from "./core/terminal";
import { openLocation, OpenLocationError } from "./core/open-location";
import type { HeaderSegment } from "./core/terminal";
import { buildFeatures, createBackgroundWiring, featureContextFor } from "./core/background-wiring";
import type { InboxStore } from "./core/background-wiring";
import { defaultWatcherIo } from "./core/watcher-runner";
import { llmProxyFor } from "./core/llm-proxy";
import { createMcpHandlers } from "./core/mcp";
import { resumeSessionInTerminal } from "./core/resume-session";
import { featureActionsHandler } from "./core/feature-actions";

let config: Config;
try {
  config = loadConfig();
} catch (err) {
  if (!(err instanceof ConfigMissingError)) throw err;
  console.error(`[startup] FAIL - ${err.message}`);
  // Exit 0 so launchd (KeepAlive: SuccessfulExit=false) doesn't respawn
  // this every few seconds — setup restarts the service once it has
  // written config.json.
  process.exit(0);
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const historyRecord = require("./core/history-record.js") as {
  createRecorder(history: HistoryStore, opts?: { log?: (m: string) => void }): { onTransition(job: Job): void; freeze(): void };
  expiredEvent(job: Job, at: number): Parameters<HistoryStore["recordEvent"]>[0] | null;
  usedEvent(job: Job | undefined, at: number): Parameters<HistoryStore["recordEvent"]>[0] | null;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("./core/paths.js") as {
  stateDir(): string;
  legacyStateDir(): string;
  migrateStateDir(): { migrated: boolean; repaired: string[]; errors: string[] };
  shouldMigrate(opts: { isStableInstall: boolean }): boolean;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const jobFiles = require("./core/job-files.js") as {
  jobsDir(stateDir: string): string;
  featureIdsToPersist(ids: string[]): string[];
  PRUNE_AFTER_MS: number;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const sessionCwd = require("./core/session-cwd.js") as {
  sessionCwdAllowed(cwd: string, opts: { repoPaths: string[]; sessionsRoot: string; checkoutsRoot: string }): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const worktreePrune = require("./core/worktree-prune.js") as {
  removePrunedWorktree(
    job: Job,
    deps: { removeWorktree: (h: WorktreeHandle) => Promise<void>; rmSync: typeof fs.rmSync; worktreeRoot: string },
  ): Promise<string>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const scopeKey = require("./core/scope-key.js") as {
  scopeKeyFor(featureId: string, payload: unknown): string | null;
  isValidScopeKey(s: unknown): boolean;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { baselines } = require("./core/integrity-baselines.js") as {
  baselines: { setDir(dir: string): void };
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const updater = require("./core/updater.js") as {
  installedVersion(): string | null;
  checkForUpdate(opts?: { maxAgeMs?: number }): Promise<unknown>;
  getUpdateInfo(opts: {
    refresh?: boolean;
    underLaunchd?: boolean;
    runningVersion?: string | null;
  }): Promise<Record<string, unknown>>;
  startUpdate(opts: { servicePid?: number | null }): Record<string, unknown>;
  cannotApplyReason(): string | null;
};

// One-time move off the old bitbucket-ai-companion-branded state dir onto
// ~/.ai-dev-companion, before anything else (worktrees, FEATURES,
// updater state) ever touches either path — see core/paths.js. Gated to
// the stable install (never a dev checkout — reusing updater's own
// "can this copy update itself" check, since a checkout that can't update
// itself is exactly the same "not the installed copy" condition) and to an
// explicit operator opt-out; see core/paths.js's shouldMigrate. Running
// this from a dev checkout would move (or worse, half-move, if a real
// install is concurrently running from it) state out from under a live
// installed copy that isn't this process.
const isStableInstall = updater.cannotApplyReason() === null;
if (paths.shouldMigrate({ isStableInstall })) {
  const migration = paths.migrateStateDir();
  if (migration.migrated) {
    console.log(`[startup] OK - moved state from ${paths.legacyStateDir()} to ${paths.stateDir()}`);
  }
  for (const migrationError of migration.errors) {
    console.log(`[startup] WARN - ${migrationError}`);
  }
} else {
  console.log("[startup] OK - skipped state-dir migration (not the installed copy)");
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const prereqs = require("./core/prereqs.js") as {
  CHECKS_BY_NAME: Record<string, () => { ok: boolean; message: string }>;
  checkRepoPath(repoPath: string): { ok: boolean; message: string };
  detectInstalledEditors(): Array<{ id: string; label: string }>;
  defaultSearchRoots(): string[];
  discoverLocalClones(roots: string[]): Array<{ key: string; path: string }>;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const registry = require("./core/feature-registry.js") as {
  REVIEW_EDITORS: Array<{ id: string; label: string }>;
  enabledFeatureIds(config: Config): string[];
  requiredChecksFor(ids: string[]): string[];
  needsRepos(ids: string[]): boolean;
};
/** What core/settings.js's publicSettings needs from the credential store
 * — see credentialsForSettings below. mergeSettings itself no longer
 * takes a credentials param at all (Fix round 1) — it returns pending
 * token operations instead of applying them; see the PUT /settings
 * handler for where those actually get applied. */
interface CredentialsForSettings {
  list(): string[];
  externalSources(config: Config): Record<string, string>;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const targets = require("./core/targets.js") as {
  analyzeTargetsFrom(config: Config): { projects: string[]; issueTypes: string[] };
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const settings = require("./core/settings.js") as {
  LOCKED_KEYS: string[];
  publicSettings(config: Config, credentials: CredentialsForSettings): Record<string, unknown>;
  featureSummaries(): Array<Record<string, unknown>>;
  validateSettingsUpdate(update: unknown, current: Config): Array<{ field: string; message: string }>;
  mergeSettings(
    current: Config,
    update: unknown,
    opts?: { savedTokenNames?: string[]; newMcpToken?: () => string },
  ): { config: Config; tokenOps: TokenOp[] };
  applyTokenOps(
    tokenOps: TokenOp[],
    setToken: (name: string, value: string | null) => void,
  ): { ok: true } | { ok: false; failedOp: TokenOp; error: Error };
  prerequisiteWarnings(ids: string[]): Array<{ check: string; message: string; features: string[] }>;
  runAllChecks(): Record<string, { ok: boolean; message: string }>;
  pendingRestart(runningEnabledIds: string[], savedConfig: Config): Record<string, unknown>;
  suggestedRepos(
    existingRepos: Record<string, string>,
    clones: Array<{ key: string; path: string }>,
  ): Array<{ key: string; path: string }>;
  migrateTokensToStore(
    config: Config,
    store: { set(name: string, value: string): void },
    saveConfig: (config: Config) => void,
  ): { migrated: string[] };
};

/** A pending token write/clear mergeSettings reports but does not apply
 * — `value: null` means clear. See core/settings.js's mergeSettings doc
 * comment for why applying it is the caller's job. */
interface TokenOp {
  name: string;
  value: string | null;
}

/** core/credential-store.js's real shape — see core/credentials.ts's own
 * CredentialStore interface, which this mirrors. */
interface CredentialStoreLike {
  get(name: string): string | undefined;
  set(name: string, value: string): void;
  remove(name: string): void;
  list(): string[];
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./core/credentials.js") as {
  credentialStore(): CredentialStoreLike;
  getToken(name: string): string | undefined;
  setToken(name: string, value: string | null): void;
  listTokenNames(): string[];
};
// `{ list }` — what core/settings.js's publicSettings needs to compute
// apiTokenSet. listTokenNames (core/credentials.ts -> core/token-cache.js)
// turns an undecryptable store into `[]` rather than throwing, so a
// corrupt credentials.enc degrades GET /settings to "nothing saved"
// instead of a 500 — see Fix round 1.
const credentialsForSettings: CredentialsForSettings = {
  list: () => credentials.listTokenNames(),
  // Where a borrowed token (core/external-tokens.js) would be used for each site, for the Settings status line.
  externalSources: (cfg) =>
    externalTokens.sourcesFor({ jira: providersFor(cfg).issues.baseUrl(), jenkins: providersFor(cfg).ci.baseUrl(), bitbucket: providersFor(cfg).git.baseUrl() }),
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const externalTokens = (require("./core/external-tokens.js") as {
  createExternalTokens(): { sourcesFor(baseUrls: Record<string, string>): Record<string, string> };
}).createExternalTokens();

// eslint-disable-next-line @typescript-eslint/no-var-requires
const hostGuard = require("./core/host-guard.js") as {
  createHostGuard(getPort: () => number): express.RequestHandler;
};

// Moves any jira/jenkins/bitbucket apiToken still sitting in config.json
// (from before this migration existed) into the encrypted credential
// store, blanking it in config.json — see core/settings.js's
// migrateTokensToStore. Runs once per startup, right after loadConfig,
// and before anything below reads config.jira/jenkins (in particular the
// "no jira.apiToken configured" warning further down, which now checks
// credentials.getToken instead).
const tokenMigration = settings.migrateTokensToStore(config, credentials.credentialStore(), writeConfig);
if (tokenMigration.migrated.length > 0) {
  console.log(
    `[startup] OK - moved ${tokenMigration.migrated.join(", ")} out of config.json into the encrypted ` +
      "credential store.",
  );
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const { LABEL: LAUNCHD_LABEL } = require("./core/launchd.js") as { LABEL: string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { readMcpServers, readAnalysisCache } = require("./features/analyze-issue/plan.js") as {
  readMcpServers(): string[];
  readAnalysisCache(issueKey: string): { completedAt: string } | null;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const summarizeCommentsPlan = require("./features/summarize-comments/plan.js") as {
  readSummaryCache(issueKey: string): { completedAt: string; commentCount: number } | null;
};

const enabledIds = registry.enabledFeatureIds(config);
const VERSION = updater.installedVersion();
// Only under launchd can an update restart the service by stopping it
// (KeepAlive starts the new build). The plist sets the flag; launchd itself
// sets XPC_SERVICE_NAME to the job's label, which covers older plists.
const UNDER_LAUNCHD =
  process.env.AI_DEV_COMPANION_UNDER_LAUNCHD === "1" || process.env.XPC_SERVICE_NAME === LAUNCHD_LABEL;

// Startup checks only ever warn — never exit. Under launchd this runs at
// login, when git/claude can transiently fail (keychain still locked, Xcode
// tools shim not ready); exiting then turned into a crash loop that kept
// the service unreachable, and took down every other feature with it.
// Each feature's own checks are re-run when a job starts instead (see
// assertFeatureReady), so a real problem still surfaces — on the one
// feature it affects, with a message saying how to fix it.
function runStartupChecks(): void {
  for (const name of registry.requiredChecksFor(enabledIds)) {
    const result = prereqs.CHECKS_BY_NAME[name]();
    console.log(`[startup] ${result.ok ? "OK  " : "WARN"} - ${result.message}`);
  }
  for (const [key, repoPath] of Object.entries(config.repos)) {
    const result = prereqs.checkRepoPath(repoPath);
    console.log(`[startup] ${result.ok ? "OK  " : "WARN"} - repos["${key}"]: ${result.message}`);
  }
}

runStartupChecks();

class PrerequisiteError extends Error {}

const CHECK_OK_TTL_MS = 5 * 60 * 1000;
const lastPassedAt = new Map<string, number>();

/** Throws a user-facing error if a prerequisite of `featureId` is failing
 * right now. Passing checks are cached briefly so a job start stays fast. */
function assertFeatureReady(featureId: string, label: string): void {
  for (const name of registry.requiredChecksFor([featureId])) {
    if (Date.now() - (lastPassedAt.get(name) ?? 0) < CHECK_OK_TTL_MS) continue;
    const result = prereqs.CHECKS_BY_NAME[name]();
    if (!result.ok) {
      throw new PrerequisiteError(`${label} can't run on this machine yet: ${result.message}`);
    }
    lastPassedAt.set(name, Date.now());
  }
}

if (enabledIds.includes("create-jira-subtasks") && !providersFor(config).issues.hasToken()) {
  // Not a hard failure — SSO (the browser session relayed from the
  // extension, see background.js) is the primary auth path for Jira
  // features; a token is only the fallback for when that doesn't work.
  console.warn(
    "[startup] WARN - no jira.apiToken saved; Jira features will rely entirely on the browser session " +
      "relayed from the extension (see README's Security notes). Add one in ✨ → ⚙ Settings if needed.",
  );
}

// Local history (a SQLite file in stateDir): installed copy only, like job
// persistence — a dev checkout shares the same state dir and must not write
// its test jobs into it. AI_DEV_COMPANION_HISTORY=1|0 overrides.
const historyOn =
  process.env.AI_DEV_COMPANION_HISTORY === "1" ||
  (process.env.AI_DEV_COMPANION_HISTORY !== "0" && isStableInstall);
let history: HistoryStore | undefined;
let historyRecorder: { onTransition(job: Job): void; freeze(): void } | undefined;
if (historyOn) {
  try {
    const historyFile = path.join(paths.stateDir(), "history.db");
    history = openHistoryStore(historyFile);
    historyRecorder = historyRecord.createRecorder(history, {
      log: (m: string) => console.log(`[history] WARN - ${m}`),
    });
    const recorder = historyRecorder;
    jobStore.onTransition((job) => recorder.onTransition(job));
    console.log(`[startup] OK - local history is on (${historyFile})`);
  } catch (err) {
    history = undefined;
    console.log(`[startup] WARN - couldn't open the local history: ${(err as Error).message}`);
  }
} else {
  console.log("[startup] OK - local history is off (not the installed copy)");
}

// One factory per feature id, from the loaded packs (core/packs.js). A feature
// declares its own factory in features/<id>/feature.js, and its module is only
// loaded when the feature is enabled — nothing in this file changes to add one.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const packs = require("./core/packs.js") as {
  featureFactories(): Record<string, (config: Config, deps: FeatureDeps) => Feature>;
  targetsFor(config: Config): Record<string, unknown>;
  siteSettingExtras(siteId: string): Array<{ key: string; defaultValue: unknown }>;
};
const FEATURE_FACTORIES = packs.featureFactories();
// eslint-disable-next-line @typescript-eslint/no-var-requires
const environment = require("./environment.js") as {
  branding: Record<string, unknown>;
  targets: { projects: string[] };
  siteIds(): string[];
  defaultBaseUrl(id: string): string;
  sites: { id: string; kind: string; label?: string; provider: string; tokenOnly?: boolean; withUsername?: boolean }[];
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const extensionManifest = require("./core/extension-manifest.js") as {
  originsByKind(config: Config): Record<string, string>;
  providersByKind(): Record<string, string>;
  hostsChanged(before: Config, after: Config): boolean;
  writeExtensionFiles(extensionDir: string, config: Config): boolean;
  defaultExtensionDir(): string;
};
// The LLM proxy (optional; one client, so its detection cache is
// shared) and, with the local history on, the similar-item search that
// analyze-issue's prompt, find_similar and `companion similar` use (Phase 8).
const llm = llmProxyFor(config);
// eslint-disable-next-line @typescript-eslint/no-var-requires
const similarSearchModule = require("./core/similar-search.js") as {
  createSimilarSearch(deps: { history: HistoryStore; llm: typeof llm; getConfig: () => Config }): { find: FindSimilar };
};
const similarSearch = history ? similarSearchModule.createSimilarSearch({ history, llm, getConfig: () => config }) : undefined;
const providers = providersFor(config);
const FEATURES: Feature[] = buildFeatures(enabledIds, FEATURE_FACTORIES, config, { providers, history, findSimilar: similarSearch?.find });

// Builds the session-vault singleton core/atlassian.ts's authedJsonWithHeaders
// now consults on every Jira/Jenkins/Bitbucket call (see core/auth-context.ts).
// `config` is passed by reference and read live, so a sessionCache change
// saved from the panel takes effect immediately — no restart needed.
configureSessionVault(config);

/** An expired job: remove its worktree and note the expiry in the history. Never throws. */
async function cleanUpExpiredJob(job: Job): Promise<void> {
  try {
    const how = await worktreePrune.removePrunedWorktree(job, {
      removeWorktree,
      rmSync: fs.rmSync,
      worktreeRoot: WORKTREE_ROOT,
    });
    if (how === "removed" || how === "removed-plain") {
      console.log(`[jobs] removed the worktree of expired job ${job.id}`);
    }
  } catch (err) {
    console.log(`[jobs] WARN - couldn't remove the worktree of expired job ${job.id}: ${(err as Error).message}`);
  }
  try {
    const event = historyRecord.expiredEvent(job, Date.now());
    if (event) history?.recordEvent(event);
  } catch (err) {
    console.log(`[history] WARN - couldn't record the expiry of job ${job.id}: ${(err as Error).message}`);
  }
}

// Persist jobs (and their worktree-integrity baselines) to stateDir/jobs and
// reload them at startup. Jobs of every feature persist except those whose
// feature.js says `persist: false` (see core/job-files.js featureIdsToPersist).
// Gated to the installed copy: a dev checkout
// shares the state dir, so it must not reconcile or prune the live service's
// jobs. AI_DEV_COMPANION_PERSIST_JOBS=1/0 overrides.
const persistJobs =
  process.env.AI_DEV_COMPANION_PERSIST_JOBS === "1" ||
  (process.env.AI_DEV_COMPANION_PERSIST_JOBS !== "0" && isStableInstall);
if (persistJobs) {
  try {
    const dir = jobFiles.jobsDir(paths.stateDir());
    baselines.setDir(dir);
    const r = jobStore.enablePersistence({
      dir,
      worktreeRoot: WORKTREE_ROOT,
      featureIds: jobFiles.featureIdsToPersist(FEATURES.map((f) => f.id)),
      sessionCwdOk: (cwd) =>
        sessionCwd.sessionCwdAllowed(cwd, {
          repoPaths: Object.values(config.repos || {}),
          sessionsRoot: path.join(paths.stateDir(), "sessions"),
          checkoutsRoot: path.join(paths.stateDir(), "repos"),
        }),
    });
    console.log(
      `[startup] OK - restored ${r.loaded} job(s) (${r.reconciled.length} interrupted, ` +
        `${r.pruned.length} pruned, ${r.dropped.length} unreadable)`,
    );
    for (const expired of r.prunedJobs) void cleanUpExpiredJob(expired);
  } catch (err) {
    // Never let a persistence problem (e.g. an unwritable stateDir) keep
    // the service from starting — worst case, jobs just aren't restored
    // or saved this run, same as persistJobs being false.
    console.log(`[startup] WARN - couldn't enable job persistence: ${(err as Error).message}`);
  }
} else {
  console.log("[startup] OK - jobs are kept in memory only (not the installed copy)");
}

// ---- Background work: the inbox, the LLM proxy, the watchers and the scheduler ----
// State files (the inbox, the scheduler's budget and what the watchers have
// seen) live in stateDir on the installed copy only, like job persistence —
// a dev checkout shares that folder. Watchers and the digest are opt-in in
// Settings; maintenance always runs.

// eslint-disable-next-line @typescript-eslint/no-var-requires
const notificationsModule = require("./core/notifications.js") as {
  createNotificationStore(opts: { file: string | null }): InboxStore;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const scheduleRules = require("./core/schedule.js") as { notifyMinIntervalMs(config: Config): number };
const inbox = notificationsModule.createNotificationStore({
  file: persistJobs ? path.join(paths.stateDir(), "notifications.json") : null,
});

// eslint-disable-next-line @typescript-eslint/no-var-requires
const mcpAuth = require("./core/mcp-auth.js") as { generateMcpToken(): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const mcpRegistration = require("./core/mcp-registration.js") as {
  registerEverywhere(opts: {
    port: number;
    token: string | undefined;
    home: string;
    claudeAvailable: boolean;
    spawnSync: typeof spawnSync;
  }): {
    claude: { ok: boolean; message: string } | null;
    cursor: { ok: boolean; message: string } | null;
  };
};

const app = express();

// Host-header check on every route: refuse requests not addressed to
// 127.0.0.1:<port> or localhost:<port> before anything else runs.
app.use(hostGuard.createHostGuard(() => config.port));

// /mcp is its own trust boundary: a separate bearer token (mcp.token in
// credentials.enc), never the extension secret — see core/mcp.ts. Mounted
// before the global JSON parser (so its own 4 MB limit applies — the
// global parser then skips the already-parsed body) and before the
// X-Companion-Secret middleware (which would 403 it). The gate (Host +
// token) runs before that parser, so an unauthenticated body is never
// parsed; bodyErrors turns a parser rejection into a JSON-RPC error
// instead of Express's HTML stack-trace page.
const mcpHandlers = createMcpHandlers({
  config,
  features: FEATURES,
  version: VERSION || "unknown",
  port: config.port,
  getToken: () => credentials.getToken("mcp.token"),
  history,
  startFeatureJob,
  readMcpServers,
  notifications: inbox,
  findSimilar: similarSearch?.find,
});
app.use("/mcp", mcpHandlers.gate);
app.use("/mcp", express.json({ limit: "4mb" }));
app.use("/mcp", mcpHandlers.bodyErrors);
// post() already catches everything; the .catch is only a backstop so a
// stray rejection can never become an unhandled one that ends the process.
app.post("/mcp", (req, res) => {
  mcpHandlers.post(req, res).catch((err: Error) => {
    console.error(`[mcp] request failed: ${err?.message || String(err)}`);
    if (!res.headersSent) res.status(500).json({ error: "Internal error" });
  });
});
app.get("/mcp", mcpHandlers.notAllowed);
app.delete("/mcp", mcpHandlers.notAllowed);

app.use(express.json());

// Unauthenticated liveness probe for setup/doctor — reveals nothing but
// "something is listening", and sets no CORS headers, so web pages can't
// read it.
app.get("/health", (_req, res) => {
  res.json({ ok: true, version: VERSION, features: FEATURES.map((f) => f.id) });
});

// Local-trust-boundary check: only a caller that knows sharedSecret (i.e.
// this machine's copy of the extension — see chrome-extension/background.js)
// may drive this service. Combined with binding to 127.0.0.1 only (see
// app.listen below), this keeps the service from being reachable by other
// localhost processes or by any webpage's fetch().
//
// This is deliberately NOT an Origin-header check: Chrome treats a fetch
// from an extension to a host declared in its manifest's host_permissions
// (http://127.0.0.1/* here) as CORS-exempt and does not send an Origin
// header for it — confirmed by driving the real extension's service worker
// and observing the request Chrome actually sends. An Origin-based
// allowlist silently rejects every legitimate request in this setup.
app.use((req: Request, res: Response, next: NextFunction) => {
  // Permissive CORS response headers are harmless here — they only matter
  // if some caller's request IS subject to CORS, and the secret check below
  // is what actually gates access either way.
  const origin = req.header("origin");
  if (origin) {
    res.header("Access-Control-Allow-Origin", origin);
  }
  res.header("Access-Control-Allow-Headers", "content-type, x-companion-secret");
  res.header("Access-Control-Allow-Methods", "GET,POST,PUT,OPTIONS");
  if (req.method === "OPTIONS") {
    res.sendStatus(204);
    return;
  }

  if (req.header("x-companion-secret") !== config.sharedSecret) {
    res.status(403).json({
      error:
        "The Chrome extension and the companion service aren't using the same secret. Reload the " +
        "extension at chrome://extensions (setup regenerates its config); if that doesn't help, re-run " +
        "`npm run setup` in the companion-service folder, then reload the extension again.",
    });
    return;
  }
  next();
});

// The state folder (worktrees, analysis clones, history, ...): its size, a
// shortcut to open it, and a purge of the rebuildable checkouts. See
// core/data-folder.js for what a purge may and may not touch.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const dataFolder = require("./core/data-folder.js") as {
  dataFolderUsage(dir: string): Promise<{ path: string; totalBytes: number; purgeableBytes: number; items: unknown[] }>;
  purgeDataFolder(dir: string): Promise<{ removed: string[]; freedBytes: number }>;
};

app.get("/data-folder", async (_req, res) => {
  try {
    res.json(await dataFolder.dataFolderUsage(paths.stateDir()));
  } catch (err) {
    sendError(res, err);
  }
});

app.post("/data-folder/open", async (_req, res) => {
  const dir = paths.stateDir();
  try {
    fs.mkdirSync(dir, { recursive: true });
    const opener = process.platform === "darwin" ? "open" : process.platform === "win32" ? "explorer" : "xdg-open";
    await run(opener, [dir]);
    res.json({ ok: true, path: dir });
  } catch (err) {
    sendError(res, err);
  }
});

app.post("/data-folder/purge", async (_req, res) => {
  // A running job is using its worktree; a restart-safe purge waits for it.
  if (jobStore.activeCount() > 0) {
    res.status(409).json({
      code: "jobs-running",
      error: "Jobs are still running. Wait for them to finish (or cancel them), then purge.",
    });
    return;
  }
  try {
    const result = await dataFolder.purgeDataFolder(paths.stateDir());
    console.log(`[data] purged ${result.removed.join(", ") || "nothing"} (${result.freedBytes} bytes)`);
    res.json({ ok: true, ...result, usage: await dataFolder.dataFolderUsage(paths.stateDir()) });
  } catch (err) {
    sendError(res, err);
  }
});

// Opens a selected stack-trace line in Cursor (the extension's
// right-click "Open in editor"). Body: { text, pageUrl? }.
app.post("/open-location", async (req, res) => {
  try {
    const body = (req.body && typeof req.body === "object" ? req.body : {}) as { text?: unknown; pageUrl?: unknown };
    res.json({ ok: true, opened: await openLocation(config, body) });
  } catch (err) {
    if (err instanceof OpenLocationError) {
      res.status(err.code === "unsupported" ? 400 : 409).json({ error: err.message, code: err.code, details: err.details });
      return;
    }
    sendError(res, err);
  }
});

app.get("/history/metrics", (req, res) => {
  if (!history) {
    res.status(404).json({ error: "Local history is off on this copy of the companion." });
    return;
  }
  const days = req.query.days === undefined ? 30 : Number(req.query.days);
  if (!Number.isInteger(days) || days < 1 || days > 3650) {
    res.status(400).json({ error: "days must be a whole number from 1 to 3650." });
    return;
  }
  res.json({ days, rows: history.metrics({ days }), prewarmed: history.prewarmMetrics({ days }) });
});

// What the extension needs to build its page patterns (which Jenkins pipelines
// and Jira projects the features cover). Reads `config` live, so a saved
// Settings change shows up on the next page refresh with no restart.
app.get("/targets", (_req, res) => {
  res.json({ ...packs.targetsFor(config), analyzeIssue: targets.analyzeTargetsFrom(config), sites: extensionManifest.originsByKind(config), providers: extensionManifest.providersByKind() });
});

// What the extension needs to know about the pull request on its page (state, conflicts, branches, fork), answered
// through the configured git provider so the extension never calls the git host's API itself — see core/pr-status.js.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const prStatusModule = require("./core/pr-status.js") as {
  createPrStatus(deps: { getGit: () => unknown }): { get(auth: AuthContext, project: string, repo: string, prId: number): Promise<unknown> };
  PrStatusRequestError: new (message: string) => Error;
};
const prStatus = prStatusModule.createPrStatus({ getGit: () => providersFor(config).git });

app.get("/prs/status", async (req, res) => {
  try {
    const { auth } = contextFor(req);
    res.json(await prStatus.get(auth, String(req.query.project || ""), String(req.query.repo || ""), Number(req.query.prId)));
  } catch (err) {
    if (err instanceof prStatusModule.PrStatusRequestError) {
      res.status(400).json({ error: err.message });
      return;
    }
    sendError(res, err);
  }
});

app.get("/features", (_req, res) => {
  res.json(FEATURES.map((f) => ({ id: f.id, label: f.label })));
});

// Whether a saved analysis exists for a ticket: the extension marks the
// "Analyze issue" menu item with a green check for as long as it does.
app.get("/analysis-cache/:issueKey", (req, res) => {
  let cached: { completedAt: string } | null = null;
  try {
    cached = readAnalysisCache(req.params.issueKey);
  } catch {
    // not an issue key: nothing saved
  }
  res.json({ exists: !!cached, completedAt: cached ? cached.completedAt : null });
});

// Same for Summarize comments: a green check while a saved summary exists.
app.get("/summary-cache/:issueKey", (req, res) => {
  let cached: { completedAt: string; commentCount: number } | null = null;
  try {
    cached = summarizeCommentsPlan.readSummaryCache(req.params.issueKey);
  } catch {
    // not an issue key: nothing saved
  }
  // commentCount lets the extension tell a current summary from a stale one.
  res.json({ exists: !!cached, completedAt: cached ? cached.completedAt : null, commentCount: cached ? cached.commentCount : null });
});

// The extension says hello whenever it's installed, reloaded or Chrome
// starts (see background.js); setup polls the GET to know Chrome is
// connected before it finishes. In memory only — a restart forgets it.
let extensionHello: { version: string | null; at: string } | null = null;

app.post("/extension/hello", (req, res) => {
  const version = typeof req.body?.version === "string" ? req.body.version : null;
  extensionHello = { version, at: new Date().toISOString() };
  // A hello carries the same X-Relay-Cookie/X-Relay-Origin headers as any
  // other request (see background.js's relayHeadersFor), so contextFor
  // caches it in the session vault exactly as it would for any other call —
  // this is what lets the very first hello after Chrome starts warm the
  // vault before any feature has run.
  contextFor(req);
  res.json({ ok: true, heartbeat: heartbeatHosts(config).heartbeat });
});

app.get("/extension/hello", (_req, res) => {
  res.json({ connected: extensionHello !== null, ...extensionHello });
});

// Which origins the extension should proactively keep a fresh cookie
// relayed for (its own heartbeat poll), even without a matching feature
// call — see core/auth-context.ts's heartbeatHosts. Behind the secret,
// like every other route below this point.
app.get("/extension/session-hosts", (_req, res) => {
  res.json(heartbeatHosts(config));
});

// Every site a feature may need a session for, whether or not the heartbeat
// is on. A feature that reads several sites (the digest) has the extension
// relay each one's cookies right before it starts — see background.js's
// relayAllSessions.
app.get("/extension/session-origins", (_req, res) => {
  res.json({ hosts: configuredOrigins(config) });
});

// What the extension needs to offer "Update to vX" — the latest version at
// the install source is cached (see core/updater.js), so polling is cheap.
app.get("/update", async (req, res) => {
  try {
    res.json(
      await updater.getUpdateInfo({
        refresh: req.query.refresh === "1",
        underLaunchd: UNDER_LAUNCHD,
        runningVersion: VERSION,
      }),
    );
  } catch (err) {
    res.status(500).json({ error: (err as Error).message });
  }
});

app.post("/update/apply", (_req, res) => {
  try {
    res.json(updater.startUpdate({ servicePid: UNDER_LAUNCHD ? process.pid : null }));
  } catch (err) {
    res.status(400).json({ error: (err as Error).message });
  }
});

// ---- Settings panel (the extension's in-browser counterpart of setup) ----
//
// Reads always go to config.json itself, so the panel shows what's saved
// even when that differs from what this process started with (see
// settings.pendingRestart for the one setting that needs a restart).

const STARTED_AT = new Date().toISOString();
const SERVICE_DIR = path.basename(__dirname) === "dist" ? path.dirname(__dirname) : __dirname;
const MANUAL_START_COMMAND = `cd ${SERVICE_DIR} && npm start`;

function savedConfig(): Config {
  try {
    return readConfigFile();
  } catch {
    return config;
  }
}

/** The proxy's last detection, waiting at most 1.5 s for a fresh one: an
 * unreachable proxy (no VPN) must not stall GET /settings for its 10 s
 * timeout. The detection keeps running and fills the cache for the next call. */
const LLM_DETECT_CAP_MS = 1500;
async function cappedLlmDetect(): Promise<unknown> {
  const detecting = llm.detect().catch(() => null);
  let timer: NodeJS.Timeout | undefined;
  const cap = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), LLM_DETECT_CAP_MS);
  });
  try {
    return await Promise.race([detecting, cap]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function settingsView(saved: Config): Promise<Record<string, unknown>> {
  const repos = saved.repos || {};
  const searchRoots = [...prereqs.defaultSearchRoots(), ...Object.values(repos).map((p) => path.dirname(p))];
  return {
    settings: settings.publicSettings(saved, credentialsForSettings),
    lockedSettings: settings.LOCKED_KEYS,
    features: settings.featureSummaries(),
    checks: settings.runAllChecks(),
    editors: { supported: registry.REVIEW_EDITORS, detected: prereqs.detectInstalledEditors() },
    repoStatus: Object.fromEntries(Object.entries(repos).map(([key, p]) => [key, prereqs.checkRepoPath(p)])),
    suggestedRepos: settings.suggestedRepos(repos, prereqs.discoverLocalClones(searchRoots)),
    branding: environment.branding,
    // The profile's sites, so the panel can show each one's connection settings under its own name.
    sites: environment.sites.map((s) => ({ id: s.id, kind: s.kind, label: s.label, provider: s.provider, tokenOnly: s.tokenOnly === true, withUsername: s.withUsername === true })),
    defaults: {
      jiraBaseUrl: DEFAULT_JIRA_BASE_URL,
      jenkinsBaseUrl: DEFAULT_JENKINS_BASE_URL,
      bitbucketBaseUrl: DEFAULT_BITBUCKET_BASE_URL,
      // Every site of the profile, by id (jira, jenkins, bitbucket, github, ...): what a blank base URL falls back to.
      siteBaseUrls: Object.fromEntries(environment.siteIds().map((id) => [id, environment.defaultBaseUrl(id)])),
      // What a blank field falls back to, for placeholders and hints: the packs' own Jenkins settings
      // (pipelines, automation jobs) and the profile's Jira projects.
      jenkinsExtras: Object.fromEntries(packs.siteSettingExtras("jenkins").map((extra) => [extra.key, extra.defaultValue])),
      analyzeProjects: environment.targets.projects,
    },
    restart: {
      ...settings.pendingRestart(enabledIds, saved),
      canRestart: UNDER_LAUNCHD,
      manualCommand: MANUAL_START_COMMAND,
      activeJobs: jobStore.activeCount(),
    },
    service: { version: VERSION, startedAt: STARTED_AT },
    background: { scheduler: scheduler.status(), llmProxy: await cappedLlmDetect() },
  };
}

/** Makes a saved config the one every feature reads from. Features hold on
 * to this same `config` object, so it's updated in place rather than
 * replaced. port/sharedSecret stay as started: the listener and the
 * secret check can't follow a change without a restart anyway. */
function applySavedConfig(next: Config): void {
  const running = config as unknown as Record<string, unknown>;
  const incoming = JSON.parse(JSON.stringify(next)) as Record<string, unknown>;
  for (const key of Object.keys(running)) {
    if (!settings.LOCKED_KEYS.includes(key) && !(key in incoming)) delete running[key];
  }
  for (const [key, value] of Object.entries(incoming)) {
    if (!settings.LOCKED_KEYS.includes(key)) running[key] = value;
  }
  if (!config.repos) config.repos = {};
}

/** Re-registers the companion's MCP server (with the current mcp.token)
 * in the MCP clients it was registered with, after a rotate — see
 * core/mcp-registration.js. Its result goes back to the panel as
 * `mcpRegistration` (null: nothing to report, e.g. the token somehow
 * still isn't readable right after the rotation that's supposed to have
 * just saved it). Run synchronously, unlike everything else in this
 * request handler: this is a rare, admin-initiated path (only reached
 * right after a successful mcp.token rotation from Settings), so
 * spawnSync's own 20s timeout blocking the event loop for that long is an
 * acceptable trade against the complexity of threading this through
 * async/await here. */
function reRegisterMcp(): {
  claude: { ok: boolean; message: string } | null;
  cursor: { ok: boolean; message: string } | null;
} | null {
  const token = credentials.getToken("mcp.token");
  if (!token) return null;
  return mcpRegistration.registerEverywhere({
    port: config.port,
    token,
    home: os.homedir(),
    claudeAvailable: prereqs.CHECKS_BY_NAME.claudeCli().ok,
    spawnSync,
  });
}

app.get("/settings", async (_req, res) => {
  try {
    res.json(await settingsView(savedConfig()));
  } catch (err) {
    res.status(500).json({ error: (err as Error).message });
  }
});

app.put("/settings", async (req, res) => {
  let saved: Config;
  try {
    saved = readConfigFile();
  } catch (err) {
    res.status(500).json({ error: `Could not read config.json: ${(err as Error).message}` });
    return;
  }
  const update = req.body as { acknowledgeWarnings?: unknown };
  const errors = settings.validateSettingsUpdate(update, saved);
  if (errors.length > 0) {
    res.status(400).json({
      error: errors.map((e) => (e.field ? `${e.field} ${e.message}` : e.message)).join("\n"),
      code: "invalid-settings",
      errors,
    });
    return;
  }
  // mergeSettings itself is pure — it reports any token change as
  // `tokenOps` rather than saving/clearing it. That's deliberate (Fix
  // round 1): a 409 below (unacknowledged prerequisite warnings) or a
  // writeConfig failure must leave EVERYTHING unchanged, tokens
  // included, so tokenOps is only ever applied once both of those have
  // already succeeded.
  // savedTokenNames: so a legacy token still in config.json (its migration
  // failed) is moved into the store rather than dropped — see mergeSite.
  // newMcpToken: only called for mcp.rotateToken, and its value only ever
  // travels as a tokenOp into credentials.enc — never into the response.
  const { config: next, tokenOps } = settings.mergeSettings(saved, update, {
    savedTokenNames: credentialsForSettings.list(),
    newMcpToken: () => mcpAuth.generateMcpToken(),
  });
  const wasEnabled = registry.enabledFeatureIds(saved);
  const newlyEnabled = registry.enabledFeatureIds(next).filter((id) => !wasEnabled.includes(id));
  const warnings = settings.prerequisiteWarnings(newlyEnabled);
  if (warnings.length > 0 && update.acknowledgeWarnings !== true) {
    res.status(409).json({
      error: "Some newly enabled features can't run on this machine yet.",
      code: "prerequisites-failing",
      warnings,
    });
    return;
  }
  try {
    writeConfig(next);
  } catch (err) {
    res.status(500).json({ error: `Could not save config.json: ${(err as Error).message}` });
    return;
  }
  const tokenResult = settings.applyTokenOps(tokenOps, credentials.setToken);
  // config.json already reflects `next` (writeConfig above already
  // succeeded) regardless of how tokenResult comes out, so the running
  // process's in-memory config must too — applying it unconditionally is
  // what makes "Everything else was applied" below actually true (Fix
  // round 2: this used to `return` before applySavedConfig on a token
  // failure, leaving the live process on its old repos/enabledFeatures/
  // etc. until the next save or a restart, even though the file on disk
  // had already moved on).
  // The extension's manifest lists the hosts it runs on, so a changed base URL needs it rewritten
  // (and the extension reloaded) before the new host is covered.
  const hostsMoved = extensionManifest.hostsChanged(config, next);
  applySavedConfig(next);
  let extension: { reloadNeeded: boolean; error?: string } = { reloadNeeded: false };
  if (hostsMoved) {
    try {
      extensionManifest.writeExtensionFiles(extensionManifest.defaultExtensionDir(), config);
      extension = { reloadNeeded: true };
    } catch (err) {
      extension = { reloadNeeded: false, error: (err as Error).message };
    }
  }
  console.log("[settings] Saved changes from the extension's Settings panel.");
  if (!tokenResult.ok) {
    // Only the token change failed (a genuine store/disk problem; an
    // undecryptable store no longer throws here — see core/token-
    // cache.js's setToken, applied via core/settings.js's
    // applyTokenOps) — say so specifically rather than a generic 500,
    // and point back at Settings to retry just that part.
    res.status(500).json({
      error:
        `Settings were saved, but saving the API token failed: ${tokenResult.error.message}. ` +
        "Everything else was applied — try the token again from ✨ → ⚙ Settings.",
    });
    return;
  }
  // A rotated MCP token only helps once the clients registered with the
  // old one have the new one — see reRegisterMcp.
  const mcpRegistration = tokenOps.some((op) => op.name === "mcp.token") ? reRegisterMcp() : null;
  // Config is already saved and applied by the time this could fail — this
  // catch is only a backstop against something going wrong building the
  // view, matching GET /settings' own error handling below.
  try {
    res.json({ ok: true, warnings, mcpRegistration, extension, ...(await settingsView(next)) });
  } catch (err) {
    res.status(500).json({ error: (err as Error).message });
  }
});

app.post("/settings/restart", (req, res) => {
  if (!UNDER_LAUNCHD) {
    res.status(400).json({
      error:
        "The companion service isn't running in the background, so it can't restart itself. Stop it " +
        `(Ctrl+C in its terminal) and start it again: ${MANUAL_START_COMMAND}`,
    });
    return;
  }
  const activeJobs = jobStore.activeCount();
  if (activeJobs > 0 && req.body?.force !== true) {
    res.status(409).json({
      error: `${activeJobs} job(s) are still running — restarting now would stop them.`,
      code: "jobs-running",
      activeJobs,
    });
    return;
  }
  res.json({ ok: true, startedAt: STARTED_AT });
  console.log("[settings] Restarting at the Settings panel's request.");
  // Not process.exit(0): the launchd job's KeepAlive (SuccessfulExit=false)
  // only restarts a job that ends unsuccessfully, and dying from a signal
  // counts — the same way core/update-runner.js restarts it.
  setTimeout(() => process.kill(process.pid, "SIGTERM"), 300);
});

/** The job as the page sees it: a feature may re-check what it hands out (Feature.present). */
function presentJob(job: Job): Job {
  const feature = FEATURES.find((f) => f.id === job.featureId);
  return feature?.present ? feature.present(job) : job;
}

app.get("/status/:jobId", async (req, res) => {
  const job = jobStore.get(req.params.jobId);
  if (!job) {
    res.status(404).json({ error: "unknown job id" });
    return;
  }
  const feature = FEATURES.find((f) => f.id === job.featureId);
  if (!feature?.sessionStatus) {
    res.json(presentJob(job));
    return;
  }
  try {
    res.json({ ...presentJob(job), session: await feature.sessionStatus(job) });
  } catch (err) {
    res.status(500).json({ error: (err as Error).message });
  }
});

// The PR or issue key to show in a resumed session's header, when this
// job's data carries one — resolve-conflict's under payload.prId,
// analyze-issue's flat on data.issueKey (see those features' index.ts).
// Neither field is ever client-supplied at this point: it's whatever the
// feature itself already stashed on the job.
function resumeHeaderKey(job: Job): string | undefined {
  const data = job.data as { payload?: { prId?: unknown }; issueKey?: unknown } | undefined;
  const prId = (data?.payload as { prId?: unknown } | undefined)?.prId;
  if (prId !== undefined && prId !== null) return `PR #${prId}`;
  if (typeof data?.issueKey === "string" && data.issueKey) return data.issueKey;
  return undefined;
}

/** The 410 body for an expired Claude Code session, worded per feature —
 * each points at the actual button that starts a fresh one for that
 * feature rather than a one-size-fits-all "use Re-analyze" (which doesn't
 * exist on Resolve Conflict's panel). */
function expiredSessionMessage(featureId: string): string {
  const prefix = "This Claude Code session has expired (its working copy or saved transcript is gone)";
  if (featureId === "analyze-issue") return `${prefix} — click Re-analyze to start a new session.`;
  if (featureId === "resolve-conflict") {
    return `${prefix} — use Open in Claude Code to start a fresh one in this worktree.`;
  }
  return `${prefix}.`;
}

const RESUME_HEADER_RULE = "─".repeat(64);

/** The header printed above a resumed session, in the style
 * features/review-in-editor/plan.js's buildTerminalHeader uses (rule,
 * title line, rule, status line) — just the feature's label and this
 * job's PR/issue key, since there's no branch/worktree/PR-link detail to
 * show here the way a fresh review has. */
function buildResumeHeader(feature: Feature, job: Job): HeaderSegment[][] {
  const key = resumeHeaderKey(job);
  return [
    [{ text: RESUME_HEADER_RULE, style: "rule" }],
    [{ text: feature.label, style: "title" }, ...(key ? [{ text: `  ${key}`, style: "heading" as const }] : [])],
    [{ text: RESUME_HEADER_RULE, style: "rule" }],
    [{ text: "Resuming Claude Code…", style: "status" }],
    [],
  ];
}

// Generic (not per-feature) because "open a terminal at this job's
// worktree and run claude" doesn't depend on which feature created it —
// only on the de facto `data.worktree.dir` convention every worktree-based
// feature already uses (see chrome-extension/open-in-editor.js, which
// reads the same field client-side). The path itself is never
// client-supplied — it's always whatever this server already put in the
// job's own data when the job was created, so there's nothing here for a
// caller to redirect to an arbitrary path.
//
// A body of `{resume: true}` instead resumes the job's headless Claude
// session (core/jobs.ts's ClaudeSession, stamped onto job.data.claudeSession
// by analyze-issue/resolve-conflict — see core/resume.js). That session's
// id/cwd/permissionMode are read ONLY from this server-held job data, never
// from the request body: a cached analyze-issue result reads claudeSession
// back off disk (features/analyze-issue/plan.js's readAnalysisCache), so
// it's validated with resume.validateSession before anything here trusts
// it enough to build a shell command from it.
app.post("/jobs/:jobId/open-in-claude-code", async (req, res) => {
  const job = jobStore.get(req.params.jobId);
  if (!job) {
    res.status(404).json({ error: "unknown job id" });
    return;
  }

  if ((req.body as { resume?: unknown } | undefined)?.resume !== true) {
    // A Review PR job keeps its (shared) review worktree in its result, not
    // in data.worktree, which job-files treats as a per-job disposable one.
    const reviewDir = (job.result as { review?: { worktree?: unknown } } | undefined)?.review?.worktree;
    const dir =
      (job.data as { worktree?: { dir?: unknown } } | undefined)?.worktree?.dir ??
      (job.featureId === "review-in-editor" ? reviewDir : undefined);
    if (typeof dir !== "string") {
      res.status(400).json({ error: "this job has no associated worktree" });
      return;
    }
    if (!fs.existsSync(dir)) {
      res.status(410).json({
        error: "the worktree no longer exists (the job may already have been approved or discarded)",
      });
      return;
    }
    try {
      await openClaudeCodeInTerminal(dir);
      res.json({ ok: true });
    } catch (err) {
      res.status(500).json({ error: (err as Error).message });
    }
    return;
  }

  if (job.status === "running" || job.status === "approving" || job.status === "rejecting") {
    res.status(409).json({ error: `This job is still ${job.status} — wait for it to finish before resuming.` });
    return;
  }
  const session = (job.data as { claudeSession?: unknown } | undefined)?.claudeSession;
  const feature = FEATURES.find((f) => f.id === job.featureId);
  const header = feature ? buildResumeHeader(feature, job) : [];
  try {
    const jobWorktree = (job.data as { worktree?: { dir?: unknown } } | undefined)?.worktree?.dir;
    const outcome = await resumeSessionInTerminal(
      config,
      session,
      header,
      typeof jobWorktree === "string" ? jobWorktree : undefined,
    );
    if (!outcome.ok && outcome.reason === "invalid") {
      res.status(400).json({ error: "This job has no resumable Claude Code session." });
      return;
    }
    if (!outcome.ok) {
      res.status(410).json({ error: expiredSessionMessage(job.featureId) });
      return;
    }
    res.json({ ok: true });
  } catch (err) {
    res.status(500).json({ error: (err as Error).message });
  }
});

// Built per-request from the relayed-cookie headers background.js sends
// (see its relayHeadersFor) plus the request body — see core/jobs.ts's
// FeatureContext and core/atlassian.ts's authedJson for how a feature
// actually uses this.
function contextFor(req: Request): FeatureContext {
  const auth = {
    cookie: req.header("x-relay-cookie") || undefined,
    origin: req.header("x-relay-origin") || undefined,
  };
  // Cache whatever session this request relayed for later requests that
  // carry no cookie of their own (an MCP tool call from terminal Claude) —
  // see core/auth-context.ts. A no-op unless origin is one of
  // configuredOrigins(config), since X-Relay-Origin is client-supplied and
  // isn't trusted otherwise.
  recordRelayedSession(auth);
  return { auth, body: req.body };
}

// AuthSetupError's message is written to be shown to the user as-is (it
// ends up in job.error, which content.js already renders in the review
// panel) — a 400 reflects "fix your config/login", not a server bug.
function statusForError(err: unknown): number {
  return err instanceof AuthSetupError || err instanceof PrerequisiteError || err instanceof RepoSetupError
    ? 400
    : 500;
}

function sendError(res: Response, err: unknown): void {
  if (err instanceof RepoNotFoundError) {
    // The extension answers this by offering to pick the folder or clone
    // it (see content.js's openRepoSetupPanel), then retries the start.
    res
      .status(409)
      .json({ error: err.message, code: "repo-not-found", project: err.project, repo: err.repo });
    return;
  }
  if (err instanceof StartRefusedError) {
    res.status(409).json({ ...err.details, error: err.message, code: err.code });
    return;
  }
  if (err instanceof JobStatusError) {
    res.status(409).json({ error: err.message });
    return;
  }
  if (err instanceof WorktreeGoneError) {
    res.status(410).json({ error: err.message });
    return;
  }
  const code = err instanceof RepoSetupError ? err.code : undefined;
  res.status(statusForError(err)).json({ error: (err as Error).message, ...(code ? { code } : {}) });
}

/** Resolves the PR's clone before a repo-based feature starts, so a missing
 * clone always surfaces as a RepoNotFoundError here — even for features
 * that would otherwise catch it and report it as a failed job. */
function assertRepoAvailable(featureId: string, body: unknown): void {
  const b = body as { project?: unknown; repo?: unknown } | null;
  if (!registry.needsRepos([featureId]) || typeof b?.project !== "string" || typeof b?.repo !== "string")
    return;
  repoPath(config, b.project, b.repo);
}

/** The one place a feature actually starts, whether the click came from
 * the extension (the /features/:id/start route below) or from
 * POST /jobs/:jobId/start turning a "pending-start" job into a real one
 * (Task 6's browser Start button). Runs the same readiness/repo checks
 * either way, then tags the resulting job with the scopeKey and
 * `startedVia` GET /jobs/lookup and JobStore.lookup key off. A watcher's
 * call (`via: "watcher"`) additionally carries ctx.background, so a feature
 * that runs Claude with no click behind it gets the readOnlyBackground
 * policy; Resolve Conflict runs its own sandboxed worktree write without a
 * click but stops at "awaiting-approval" — only a person's Approve pushes. */
async function startFeatureJob(
  feature: Feature,
  payload: unknown,
  ctx: FeatureContext,
  via: "extension" | "mcp" | "watcher",
  watcher?: string,
): Promise<Job> {
  assertFeatureReady(feature.id, feature.label);
  assertRepoAvailable(feature.id, payload);
  // ctx.background is honoured only for the watcher's own call (see
  // featureContextFor), whatever a caller put in the context.
  const job = await feature.start(payload, featureContextFor(via, ctx));
  return jobStore.update(job.id, {
    scopeKey: scopeKey.scopeKeyFor(feature.id, payload) ?? undefined,
    startedVia: via,
    ...(watcher ? { watcher } : {}),
  });
}

const { scheduler, routes: inboxRoutes } = createBackgroundWiring({
  config,
  features: FEATURES,
  jobStore,
  startFeatureJob,
  inbox,
  history,
  llm,
  io: defaultWatcherIo(config),
  statePath: persistJobs ? path.join(paths.stateDir(), "scheduler.json") : null,
  maintenance: () => dailyMaintenance(),
  isShuttingDown: () => shuttingDown,
});

app.get("/notifications", inboxRoutes.list);
app.post("/notifications/seen", inboxRoutes.seen);
app.post("/notifications/announce", inboxRoutes.announce);
app.post("/notifications/:id/open", inboxRoutes.open);

app.post("/repos/choose", async (req, res) => {
  try {
    res.json({ path: await chooseRepoFolder(config, req.body) });
  } catch (err) {
    sendError(res, err);
  }
});

app.post("/repos/clone", (req, res) => {
  try {
    res.json(startClone(config, req.body, req.header("x-relay-origin") || undefined));
  } catch (err) {
    sendError(res, err);
  }
});

for (const feature of FEATURES) {
  app.post(`/features/${feature.id}/start`, async (req, res) => {
    try {
      const job = await startFeatureJob(feature, req.body, contextFor(req), "extension");
      res.json(presentJob(job));
    } catch (err) {
      sendError(res, err);
    }
  });

  app.post(`/features/${feature.id}/:jobId/approve`, async (req, res) => {
    const job = jobStore.get(req.params.jobId);
    if (!job) {
      res.status(404).json({ error: "unknown job id" });
      return;
    }
    try {
      await feature.approve(job, contextFor(req));
      res.json(presentJob(job));
    } catch (err) {
      // sendError (not a bare statusForError) so a JobStatusError — e.g.
      // resolve-conflict's reject() refusing while "approving" — maps to
      // 409 like refreshDiff's already does, instead of falling through to
      // a generic 500; it's a no-op change for every other error shape,
      // which sendError still routes through statusForError.
      sendError(res, err);
    }
  });

  app.post(`/features/${feature.id}/:jobId/reject`, async (req, res) => {
    const job = jobStore.get(req.params.jobId);
    if (!job) {
      res.status(404).json({ error: "unknown job id" });
      return;
    }
    try {
      await feature.reject(job, contextFor(req));
      res.json(presentJob(job));
    } catch (err) {
      sendError(res, err);
    }
  });

  if (feature.cancel || feature.stopSession) {
    app.post(`/features/${feature.id}/:jobId/cancel`, async (req, res) => {
      const job = jobStore.get(req.params.jobId);
      if (!job) {
        res.status(404).json({ error: "unknown job id" });
        return;
      }
      try {
        // The job may have finished between the click and this request;
        // cancelling then means discarding its result, i.e. a reject.
        if (job.status === "running" && feature.cancel) await feature.cancel(job, contextFor(req));
        else if (job.status === "approved" && feature.stopSession) {
          await feature.stopSession(job);
          res.json({ ...job, session: await feature.sessionStatus?.(job) });
          return;
        } else if (feature.cancel && (job.status === "awaiting-approval" || job.status === "failed")) {
          await feature.reject(job, contextFor(req));
        }
        res.json(job);
      } catch (err) {
        sendError(res, err);
      }
    });
  }

  if (feature.postComment) {
    const postComment = feature.postComment.bind(feature);
    app.post(`/features/${feature.id}/:jobId/post-comment`, async (req, res) => {
      const job = jobStore.get(req.params.jobId);
      if (!job) {
        res.status(404).json({ error: "unknown job id" });
        return;
      }
      try {
        const result = await postComment(job, contextFor(req));
        res.json({ ok: true, ...result });
      } catch (err) {
        res.status(statusForError(err)).json({ error: (err as Error).message });
      }
    });
  }

  if (feature.actions) {
    app.post(
      `/features/${feature.id}/:jobId/actions/:action`,
      featureActionsHandler(feature, { contextFor, sendError, present: presentJob }),
    );
  }

  if (feature.refreshDiff) {
    const refreshDiff = feature.refreshDiff.bind(feature);
    app.post(`/features/${feature.id}/:jobId/refresh-diff`, async (req, res) => {
      const job = jobStore.get(req.params.jobId);
      if (!job) {
        res.status(404).json({ error: "unknown job id" });
        return;
      }
      try {
        res.json(await refreshDiff(job, contextFor(req)));
      } catch (err) {
        sendError(res, err);
      }
    });
  }
}

// GET, not behind any per-route auth beyond the shared-secret middleware
// every route already sits behind — a page just wants to know if it
// already has a job before offering to create one via an MCP tool (Task
// 9), which is read-only. featureId must name a currently running
// feature (not just any string) so this can't be used to probe for
// features that exist but aren't enabled.
app.get("/jobs/lookup", (req, res) => {
  const scopeKeyParam = req.query.scopeKey;
  const featureId = req.query.featureId;
  const feature = typeof featureId === "string" ? FEATURES.find((f) => f.id === featureId) : undefined;
  if (typeof scopeKeyParam !== "string" || !scopeKey.isValidScopeKey(scopeKeyParam) || !feature) {
    res.status(400).json({ error: "scopeKey and featureId (a running feature's id) are required" });
    return;
  }
  const found = jobStore.lookup(scopeKeyParam, feature.id);
  res.json({ job: found ? presentJob(found) : found });
});

// The two routes below turn a "pending-start" job (created by an MCP tool
// via JobStore.createPending — see Task 9) into a real one, or discard it
// — only ever from a browser click, never from terminal Claude itself, so
// they sit behind the same shared-secret check as every other POST here.
// That's the whole point of "pending-start": a tool that would give
// Claude Bash access can't run without this click first.
// Pending job ids currently mid-flight through the route below, between
// the request landing and startFeatureJob resolving. The pending job
// itself only turns "rejected" *after* that resolves (see the comment
// inside the handler), so it can't be what blocks a second, overlapping
// request for the same id — e.g. a double click the extension's own
// in-flight guard missed, or a second browser tab. Nothing about a
// Feature's start() actually guarantees it returns before its first
// await today, so this can't just rely on the synchronous part of the
// handler either.
const jobsBeingStarted = new Set<string>();

app.post("/jobs/:jobId/start", async (req, res) => {
  const job = jobStore.get(req.params.jobId);
  if (!job) {
    res.status(404).json({ error: "unknown job id" });
    return;
  }
  if (job.status !== "pending-start") {
    res.status(409).json({ error: `This job is "${job.status}", not waiting to be started.` });
    return;
  }
  if (jobsBeingStarted.has(job.id)) {
    res.status(409).json({ error: "This job is already being started." });
    return;
  }
  const feature = FEATURES.find((f) => f.id === job.featureId);
  if (!feature) {
    res.status(400).json({ error: `Feature "${job.featureId}" isn't enabled.` });
    return;
  }
  jobsBeingStarted.add(job.id);
  try {
    const newJob = await startFeatureJob(feature, (job.data as { payload?: unknown }).payload, contextFor(req), "mcp");
    // Only reached once startFeatureJob has actually succeeded — a failed
    // start must leave the pending job exactly as it was, so the user can
    // still see and retry it rather than it silently turning "rejected".
    jobStore.update(job.id, { status: "rejected" });
    jobStore.patchData(job.id, { supersededBy: newJob.id });
    res.json(newJob);
  } catch (err) {
    sendError(res, err);
  } finally {
    jobsBeingStarted.delete(job.id);
  }
});

app.post("/jobs/:jobId/dismiss", (req, res) => {
  const job = jobStore.get(req.params.jobId);
  if (!job) {
    res.status(404).json({ error: "unknown job id" });
    return;
  }
  if (job.status !== "pending-start") {
    res.status(409).json({ error: `This job is "${job.status}", not waiting to be started.` });
    return;
  }
  jobStore.update(job.id, { status: "rejected" });
  res.json({ ok: true });
});

// Graceful shutdown: take the detached headless claude runs down with us.
// core/exec.ts starts each one in its own process group, so the SIGTERM
// that stops this process (the Settings restart below, the updater,
// `launchctl bootout`) never reaches them on its own — an orphaned claude
// would keep writing its worktree while the restarted service offers
// Refresh diff and Approve on it. (A crash or `kill -9` skips this
// handler entirely; the features' `data.claudePid` check covers that.)
const SHUTDOWN_WAIT_MS = 2000;
let shuttingDown = false;
const stopFeatureBackground: Array<() => void> = [];

function onShutdownSignal(sig: NodeJS.Signals): void {
  // A second signal while we're already waiting changes nothing: the
  // first one's hard timeout below still ends the process.
  if (shuttingDown) return;
  shuttingDown = true;
  // Frozen first, so the jobs these kills fail are never saved as
  // "failed: aborted" — their files keep "running" and the next start
  // reconciles them to the restart message (core/job-files.js).
  jobStore.freezePersistence();
  historyRecorder?.freeze();
  scheduler.stop();
  for (const stop of stopFeatureBackground) stop();
  killDetachedChildren("SIGTERM");
  const deadline = Date.now() + SHUTDOWN_WAIT_MS;
  const finish = () => {
    // Re-raise the same signal with our handlers gone, so the exit is still
    // "killed by signal": launchd's KeepAlive (SuccessfulExit=false) only
    // restarts a job that ends unsuccessfully, which is how the Settings
    // restart and core/update-runner.js bring the service back.
    process.removeListener("SIGTERM", onShutdownSignal);
    process.removeListener("SIGINT", onShutdownSignal);
    // Backstop in case something else still listens for `sig` and the
    // re-raise doesn't end us: a non-zero exit is also "unsuccessful" to
    // launchd, so it still restarts the service.
    setTimeout(() => process.exit(1), 1000);
    process.kill(process.pid, sig);
  };
  // Bounded: a child that ignores SIGTERM must never keep the service
  // from restarting. Polled rather than event-driven so a missed exit
  // event can't hang this either.
  const poll = () => {
    if (detachedChildCount() === 0 || Date.now() >= deadline) {
      finish();
      return;
    }
    setTimeout(poll, 50);
  };
  poll();
}
process.on("SIGTERM", onShutdownSignal);
process.on("SIGINT", onShutdownSignal);

/** The scheduler's "maintenance" task, a minute after start and then daily:
 * expire stale jobs (removing their worktrees) and prune the history past
 * its retention. Never throws; its counts go into the run's events row. */
async function dailyMaintenance(): Promise<Record<string, unknown>> {
  let expired = 0;
  try {
    for (const job of jobStore.sweepStale(jobFiles.PRUNE_AFTER_MS)) {
      await cleanUpExpiredJob(job);
      expired++;
    }
    const pruned = history?.prune(config.history?.retentionDays ?? 180);
    if (pruned && (pruned.items > 0 || pruned.events > 0)) {
      console.log(`[history] pruned ${pruned.items} item(s) and ${pruned.events} event(s) past the retention window`);
    }
    return { expired, prunedItems: pruned?.items ?? 0, prunedEvents: pruned?.events ?? 0 };
  } catch (err) {
    console.log(`[maintenance] WARN - ${(err as Error).message}`);
    return { outcome: "failed", expired, error: (err as Error).message };
  }
}

const server = app.listen(config.port, "127.0.0.1", () => {
  console.log(`ai-dev-companion companion v${VERSION} listening on http://127.0.0.1:${config.port}`);
  console.log(`Features: ${FEATURES.map((f) => f.id).join(", ")}`);
  const checkForUpdate = () => void updater.checkForUpdate();
  setTimeout(checkForUpdate, 15000).unref();
  setInterval(checkForUpdate, 6 * 60 * 60 * 1000).unref();
  // Maintenance, the watchers and the digest all run from the scheduler.
  scheduler.start();
  if (persistJobs) {
    for (const f of FEATURES) {
      const stop = f.startBackground?.();
      if (stop) stopFeatureBackground.push(stop);
    }
  }
  if (!config.sharedSecret || config.sharedSecret.startsWith("REPLACE_WITH")) {
    console.warn("sharedSecret is unset — every request will be rejected until config.json is fixed.");
  }
});

server.on("error", (err: NodeJS.ErrnoException) => {
  if (err.code === "EADDRINUSE") {
    console.error(
      `[startup] FAIL - port ${config.port} is already in use by another process. Either stop that ` +
        `process (\`lsof -nP -iTCP:${config.port} -sTCP:LISTEN\` shows which), or change "port" in ` +
        `config.json and re-run \`npm run setup\` so the extension picks up the new port.`,
    );
  } else {
    console.error(`[startup] FAIL - could not start the HTTP server: ${err.message}`);
  }
  process.exit(1);
});
