import * as fs from "fs";
import * as path from "path";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const prereqs = require("./core/prereqs.js") as {
  inferRepoPath(
    existingRepos: Record<string, string>,
    project: string,
    repo: string,
    extraRoots?: string[],
  ): string | null;
  defaultSearchRoots(): string[];
};
/** Thrown when config.json is missing or unreadable — nothing can run
 * until setup has been, so server.ts exits cleanly instead of crash-looping. */
export class ConfigMissingError extends Error {}

/** No local clone of project/repo could be found. server.ts turns this into
 * a 409 the extension answers by offering to pick a folder or clone it. */
export class RepoNotFoundError extends Error {
  constructor(
    public readonly project: string,
    public readonly repo: string,
    message: string,
  ) {
    super(message);
  }
}

export interface Config {
  port: number;
  /**
   * Shared secret the extension's background.js sends as the
   * X-Companion-Secret header on every request. NOT an Origin check:
   * Chrome treats fetches from an extension to a host declared in its
   * manifest's host_permissions as CORS-exempt and does not send an
   * Origin header for them (confirmed empirically — see git history /
   * PR description for how this was diagnosed), so Origin can't be used
   * to identify the caller.
   */
  sharedSecret: string;
  /** "PROJECT/repo" -> absolute path of an existing local clone. */
  repos: Record<string, string>;
  /**
   * Jira access for features like create-jira-subtasks. SSO (the browser
   * session relayed from the extension — see background.js) is tried
   * first; apiToken is only the fallback for when that doesn't work (e.g.
   * this service is run under a different login than the browser, or the
   * relayed session has expired). See core/atlassian.ts.
   */
  jira?: {
    /** Defaults to DEFAULT_JIRA_BASE_URL (environment.js) if omitted. */
    baseUrl?: string;
    apiToken?: string;
  };
  /**
   * Which feature ids to actually register (see core/feature-registry.js
   * for the full descriptor list setup.js drives itself off of). Absent
   * means "every known feature" — an existing config.json from before
   * this field existed keeps behaving exactly as it did, rather than
   * silently losing a feature nobody asked to disable.
   */
  enabledFeatures?: string[];
  /**
   * Snapshot of feature ids that existed the last time config.json was
   * written (by setup or by loadConfig's soft-migrate). Used with
   * enabledFeatures so a newly shipped feature with enabledByDefault
   * omit/true is auto-enabled for existing installs, while a feature the
   * user unchecked in setup (or one shipped with enabledByDefault:false)
   * stays off across later updates. See core/feature-registry.js's
   * migrateEnabledFeatures.
   */
  knownFeatures?: string[];
  /**
   * Which editor "Review PR" opens the PR-review worktree in.
   * Optional (not normalized in loadConfig, unlike repos
   * below) — a config.json written before this field existed, or before
   * review-in-editor was ever enabled, genuinely has no opinion here, and
   * that's fine: core/editor.ts's runtime fallback auto-detects an
   * installed editor when this is unset.
   */
  reviewEditor?: "vscode" | "cursor" | "claude-code";
  /**
   * Jenkins access for pre-deployment-stats. Same SSO-first arrangement as
   * `jira` above: the browser session relayed from the extension is tried
   * first, and username+apiToken (Jenkins' HTTP Basic convention, where
   * the password half is an API token) is only the fallback. See
   * core/jenkins.ts.
   *
   * Entirely optional, and deliberately not normalized in loadConfig —
   * every reader goes through core/jenkins.ts / the feature's own
   * defaults, so a config.json written before this existed, or one whose
   * owner never enabled the feature, is fine as-is. Adding a required
   * field here is what caused the crash-loop fixed in 693b364.
   */
  jenkins?: {
    /** Defaults to DEFAULT_JENKINS_BASE_URL if omitted. */
    baseUrl?: string;
    username?: string;
    apiToken?: string;
    /** How many successful helm builds to examine before giving up on
     * finding a green one. Read by a pack that scans builds (the default lives with the pack). */
    maxBuildsToScan?: number;
    /** How far back to walk when dating a failure whose test name changes
     * every run. Read by a pack that scans builds (the default lives with the pack). */
    maxWalkBackBuilds?: number;
    /** The pipelines Pre-deployment stats can scan and the automation jobs checked
     * per build. Owned by the pack that uses them (e.g. a distribution's pack), which
     * validates them; the core only stores them. */
    pipelines?: unknown;
    autJobs?: unknown;
  };
  /**
   * Optional overrides for Analyze ticket. Entirely optional — readers fall
   * back to Claude Code's default model and an empty componentRepoMap when absent, so a
   * config.json written before this feature existed stays valid.
   */
  analyzeIssue?: {
    /** Passed as `claude --model` when set; omitted means Claude Code's default. */
    model?: string;
    /** Jira component/label name -> "PROJECT/repo" key in config.repos. */
    componentRepoMap?: Record<string, string>;
    /** Jira project keys Analyze ticket accepts. Defaults to the profile's `targets.projects`. */
    projects?: string[];
    /** Jira issue types Analyze ticket accepts. Defaults to ["Bug"]. */
    issueTypes?: string[];
  };
  /** GitHub or GitHub Enterprise, when the profile's git site is GitHub: the web address (blank = github.com);
   * the token lives in the credential store as github.apiToken. */
  github?: {
    baseUrl?: string;
  };
  /**
   * Bitbucket access for Address review comments (Task 6, not built yet).
   * Same SSO-first arrangement as `jira`/`jenkins` above — see
   * core/bitbucket.ts. Entirely optional and deliberately not normalized
   * in loadConfig, same reasoning as `jenkins`: every reader falls back to
   * bitbucketSite()'s own default, so a config.json written before this
   * field existed is fine as-is.
   */
  bitbucket?: {
    /** Defaults to DEFAULT_BITBUCKET_BASE_URL if omitted. */
    baseUrl?: string;
  };
  /** How long the local history keeps things; see core/history-schema.js. */
  history?: { retentionDays?: number };
  /** Optional shared test-history feed (risk-facts/v1); see core/risk-facts.js. */
  riskFacts?: { url?: string };
  /** Ticket to PR (features/ticket-to-pr). `reviewTransitionName` is the
   * Jira transition (or target status) the ticket is moved with once its pull
   * request is open; defaults to "In Review", skipped when the ticket has none
   * by that name. `autoMoveToReview` (default on; only `false` turns it off)
   * lets the service do that by itself when it sees the PR. */
  ticketToPr?: { reviewTransitionName?: string; autoMoveToReview?: boolean };
  /** Summarize comments: the Claude model that writes the summary. Defaults
   * to DEFAULT_SUMMARIZE_COMMENTS_MODEL (Haiku) — a short summary needs no more. */
  summarizeComments?: { model?: string };
  /** Background watchers (core/watchers.js, run by core/scheduler.ts). Each
   * is off unless `enabled`; core/schedule.js has the default intervals. */
  watchers?: Partial<
    Record<"conflicts" | "assignedBugs" | "reviewRequests", { enabled?: boolean; intervalMinutes?: number }>
  >;
  /** Background Claude runs per local day (default 5; 0 = none). */
  budget?: { claudeRunsPerDay?: number };
  /** Optional quiet hours, e.g. 19:00-08:00: no watcher runs and no Chrome
   * notifications; what they hold goes into the morning digest. */
  scheduler?: { quietHours?: { start: string; end: string } };
  /** At most one Chrome notification per this many minutes, urgent ones
   * aside (default 30). */
  notify?: { minIntervalMinutes?: number };
  /** The scheduled weekday morning digest: off unless enabled; 08:30 by default. */
  digest?: { enabled?: boolean; time?: string };
  /** The OpenAI-compatible LLM proxy (core/llm-proxy.ts). The key lives in
   * credentials.enc as `llmProxy.apiKey`, never here. `allowedModels`
   * (default: the on-prem models) is only ever changed by hand. */
  llmProxy?: { baseUrl?: string; user?: string; chatModel?: string; embeddingModel?: string; allowedModels?: string[] };
  /** `companion precheck`: "warn" (default) prints cited warnings, "off" is
   * silent. It never blocks a push. */
  prePush?: { mode?: "warn" | "off" };
  /**
   * In-memory only; see core/session-vault.js. Controls the cache of
   * browser sessions relayed from the extension (X-Relay-Cookie /
   * X-Relay-Origin), so an MCP tool call from terminal Claude — which
   * carries no cookie of its own — can still act as the logged-in user
   * for a while. Never written to disk anywhere but here; nothing about
   * a cookie itself is ever persisted. See core/auth-context.ts.
   */
  sessionCache?: {
    /** Defaults to 30 if omitted. 0 turns the cache off entirely. */
    ttlMinutes?: number;
    /** Defaults to false. When true, the extension is told (via
     * GET /extension/session-hosts) to periodically relay a fresh cookie
     * for each configured site even without a matching feature call. */
    heartbeat?: boolean;
  };
  /**
   * Optional overrides for Address review comments. Entirely optional,
   * same reasoning as `analyzeIssue` above.
   */
  addressReviewComments?: {
    /** Model for the run: a Claude model id, or a Cursor model when Cursor
     * is the default editor. Unset = the tool's own default. */
    model?: string;
    /** Extra commands Claude may run to check its edits (e.g. "npm test"),
     * on top of `git diff`/`git status`. core/bash-guard.js allows each
     * only as that command alone or followed by arguments — never chained,
     * piped or redirected — and core/claude-args.js's
     * normalizeCheckCommands refuses entries containing shell
     * metacharacters, `*` or line breaks. */
    checkCommands?: string[];
  };
}

export const DEFAULT_SUMMARIZE_COMMENTS_MODEL = "claude-haiku-4-5-20251001";
// eslint-disable-next-line @typescript-eslint/no-var-requires
const environment = require("./environment.js") as { defaultBaseUrl(id: string): string };
export const DEFAULT_JIRA_BASE_URL = environment.defaultBaseUrl("jira");
export const DEFAULT_JENKINS_BASE_URL = environment.defaultBaseUrl("jenkins");
export const DEFAULT_BITBUCKET_BASE_URL = environment.defaultBaseUrl("bitbucket");

// Always companion-service/config.json itself — *never* a separate
// dist/config.json copy. This used to be plain `path.join(__dirname,
// "config.json")`, which resolves to dist/config.json once compiled
// (since __dirname is dist/ for the running server) — a second copy the
// build script had to remember to refresh (`cp config.json
// dist/config.json`), and every time that step got skipped (a re-run of
// `npm run setup`, a hand edit, then a service restart without a
// rebuild in between) the running server kept reading stale settings
// indefinitely, with no error to say so. Confirmed the hard way, more
// than once, so there's now only ever one file to go stale.
const CONFIG_PATH = path.join(__dirname, path.basename(__dirname) === "dist" ? ".." : ".", "config.json");

export function loadConfig(): Config {
  let config: Config;
  try {
    config = JSON.parse(fs.readFileSync(CONFIG_PATH, "utf8")) as Config;
  } catch (err) {
    const reason =
      (err as NodeJS.ErrnoException).code === "ENOENT"
        ? "doesn't exist yet"
        : `can't be read (${(err as Error).message})`;
    throw new ConfigMissingError(
      `${CONFIG_PATH} ${reason}. Run the setup wizard first: ` +
        `cd ${path.dirname(CONFIG_PATH)} && npm run setup`,
    );
  }

  if (!config.sharedSecret || config.sharedSecret.startsWith("REPLACE_WITH")) {
    console.warn(
      "[config] sharedSecret is not set in config.json — every request will be " +
        "rejected until you set it (and set the same value in chrome-extension/background.js).",
    );
  }

  // `repos` is only ever written by resolve-conflict's own
  // promptSetup (see core/feature-registry.js) — so a config.json written
  // while that feature was never enabled genuinely has no key at
  // all, even though this type declares it required. Normalized here,
  // once, so every other reader of `config` (runStartupChecks's
  // `Object.entries(config.repos)` crashed on exactly this before it was
  // added) can keep trusting the type instead of null-checking themselves.
  if (!config.repos) config.repos = {};

  // Soft-migrate enabledFeatures when new descriptors ship — see
  // core/feature-registry.js's migrateEnabledFeatures. Persisted so the
  // next startup (and setup --yes) don't re-log the same merge.
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const registry = require("./core/feature-registry.js") as {
    migrateEnabledFeatures(
      cfg: Config,
    ): { enabledFeatures: string[]; knownFeatures: string[] } | null;
  };
  const migration = registry.migrateEnabledFeatures(config);
  if (migration) {
    const added = migration.enabledFeatures.filter((id) => !config.enabledFeatures?.includes(id));
    Object.assign(config, migration);
    writeConfig(config);
    if (added.length > 0) {
      console.log(
        `[config] Enabled new feature(s) added since your last setup: ${added.join(", ")}. ` +
          `Re-run \`npm run setup\` if you want to turn any of them off.`,
      );
    }
  }

  return config;
}

/**
 * Resolve the local clone path for a "PROJECT/repo" key. If it isn't
 * mapped yet, tries to infer it from where your *other* configured repos
 * live (see core/prereqs.js's inferRepoPath — it only accepts a candidate
 * once it's confirmed that candidate's own git origin actually matches
 * this exact project/repo, never just "a directory with the right name
 * happens to exist"). A successful inference is persisted to config.json
 * so it doesn't need to be re-discovered on the next job. Throws only if
 * neither an explicit mapping nor a confident inference is available.
 * A mapping whose folder is no longer a git checkout (moved or deleted) is
 * dropped and resolved afresh, rather than failing on the stale path.
 */
export function repoPath(config: Config, project: string, repo: string): string {
  const key = `${project}/${repo}`;
  const existingKey = Object.keys(config.repos).find((k) => k.toLowerCase() === key.toLowerCase());
  if (existingKey) {
    const mapped = config.repos[existingKey];
    if (fs.existsSync(path.join(mapped, ".git"))) return mapped;
    console.warn(
      `[config] repos["${existingKey}"] = "${mapped}" is no longer a git checkout; looking again.`,
    );
    delete config.repos[existingKey];
  }

  const inferred = prereqs.inferRepoPath(config.repos, project, repo, prereqs.defaultSearchRoots());
  if (!inferred) {
    throw new RepoNotFoundError(
      project,
      repo,
      `No local clone of "${key}" was found (looked next to your other clones and in common folders ` +
        `like ~/gitviews, ~/git, ~/src). Choose its folder or let the assistant clone it from the ✨ ` +
        `menu, or clone it yourself next to your other repos and try again.`,
    );
  }

  saveRepo(config, key, inferred);
  console.log(`[config] Found repos["${key}"] = "${inferred}" and saved it to config.json.`);
  return inferred;
}

/** Adds (or replaces) a repo mapping and writes it to config.json, so it
 * survives a restart. The repos map is a cache the service fills itself —
 * by inference, a folder the user picked, or a clone it made. */
export function saveRepo(config: Config, key: string, clonePath: string): void {
  config.repos[key] = clonePath;
  writeConfig(config);
}

/** Remembers that tickets with this Jira component belong in this repo, so
 * the next one is matched without asking Claude. Never replaces an entry that
 * is already there; the map stays small (validateComponentRepoMap's limit). */
export function saveComponentRepo(config: Config, component: string, repoKey: string): boolean {
  const analyze = (config.analyzeIssue = config.analyzeIssue || {});
  const map = (analyze.componentRepoMap = analyze.componentRepoMap || {});
  if (Object.keys(map).length >= 50) return false;
  if (Object.keys(map).some((k) => k.toLowerCase() === component.toLowerCase())) return false;
  map[component] = repoKey;
  writeConfig(config);
  return true;
}

/** config.json exactly as saved — no normalizing or migrating, unlike
 * loadConfig. Throws if it's missing or unreadable. */
export function readConfigFile(): Config {
  return JSON.parse(fs.readFileSync(CONFIG_PATH, "utf8")) as Config;
}

/** Replaces config.json via a temp file + rename so a crash never leaves
 * a half-written file. Keeps the existing file's permissions (it holds the
 * shared secret and any API tokens); a new file is created owner-only. */
export function writeConfig(config: Config): void {
  let mode = 0o600;
  try {
    mode = fs.statSync(CONFIG_PATH).mode & 0o777;
  } catch {
    // No file yet — owner-only default.
  }
  const tmp = path.join(
    path.dirname(CONFIG_PATH),
    `.${path.basename(CONFIG_PATH)}.${process.pid}.${Date.now()}.tmp`,
  );
  try {
    fs.writeFileSync(tmp, JSON.stringify(config, null, 2) + "\n", { mode });
    fs.renameSync(tmp, CONFIG_PATH);
  } catch (err) {
    fs.rmSync(tmp, { force: true });
    throw err;
  }
}
