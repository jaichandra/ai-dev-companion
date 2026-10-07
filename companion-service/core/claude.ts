import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { run } from "./exec";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("./paths.js") as { stateDir(home?: string): string };

interface ClaudePolicy {
  tools: string[];
  mcpGuard: boolean;
  denyCwdWrite?: boolean;
  readGuard?: boolean;
  bashGuard?: boolean;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const claudeArgs = require("./claude-args.js") as {
  CLAUDE_POLICIES: Record<"readOnly" | "readOnlyBackground" | "worktreeWrite" | "worktreeWriteNarrowBash" | "noTools", ClaudePolicy>;
  protectedPathRules(opts: { stateDir: string; companionDir: string; home: string }): string[];
  protectedSandboxPaths(opts: { stateDir: string; companionDir: string; home: string }): string[];
  buildClaudeArgs(opts: {
    prompt: string;
    model?: string;
    policy: ClaudePolicy;
    sessionId?: string;
    cwd?: string;
    extraAllowedTools?: string[];
    disallowedTools?: string[];
    guardScriptPath?: string;
    protectedRules?: string[];
    sandboxDenyRead?: string[];
    sandboxAllowWrite?: string[];
    sandboxDenyWrite?: string[];
    editRoot?: string;
    editGuardScriptPath?: string;
    readGuardScriptPath?: string;
    bashGuardScriptPath?: string;
    outputFormat?: string;
    execPath?: string;
  }): string[];
  companionServiceDir(): string;
};

// eslint-disable-next-line @typescript-eslint/no-var-requires
const claudeStream = require("./claude-stream.js") as {
  createStreamAccumulator(opts?: { cwd?: string }): {
    feed(line: string): { label?: string };
    result(): { sawResult: boolean; text: string; sessionId?: string; permissionDenials: string[]; isError: boolean };
  };
  createThrottle(minMs: number): (fn: () => void) => void;
};
const PROGRESS_MIN_INTERVAL_MS = 1000;

export interface ClaudeRunOptions {
  /** Directory the CLI runs in — this is the sandbox boundary. Every file
   * edit and bash command Claude issues is scoped to this cwd (in practice,
   * a disposable git worktree — see core/worktree.ts). */
  cwd: string;
  prompt: string;
  /** Passed as `--model` only when set; unset means Claude Code's own default. */
  model?: string;
  /** Tool names to expose, e.g. ["Read", "Edit", "Write", "Bash", "Glob", "Grep"].
   * Kept explicit (rather than the full default toolset) so a feature only
   * grants what it actually needs — e.g. no WebFetch/WebSearch for a job
   * that should stay offline.
   *
   * May include permission patterns like `Bash(git log:*)` — those are
   * passed through to `--allowedTools` as-is, but `--tools` (availability)
   * only gets the base name (`Bash`), since that flag only lists built-ins.
   *
   * Optional when `policy` is given — the named policy supplies its own
   * tools list, so a caller only needs one or the other. */
  tools?: string[];
  /** A named policy from core/claude-args.js's CLAUDE_POLICIES. Supplies
   * `tools` (overriding this option's `tools`, if both are somehow given)
   * and, for policies with `mcpGuard: true`, wires in core/mcp-guard.js as
   * a PreToolUse hook so even an MCP server discovered via ~/.claude.json
   * can't run a mutating tool — not just the ones a feature's own
   * disallowedTools happens to name. */
  policy?: keyof typeof claudeArgs.CLAUDE_POLICIES;
  /** Passed to `claude --session-id <uuid>` so a later phase can resume
   * this exact transcript (e.g. an interactive handoff). */
  sessionId?: string;
  /** Extra names appended to `--allowedTools` only (not `--tools`) — e.g.
   * `mcp__<server>` prefixes so headless Claude can call user-scope MCP
   * servers loaded from ~/.claude.json. */
  extraAllowedTools?: string[];
  /** Passed as `--disallowedTools` — an explicit deny list that wins over
   * allowedTools for known mutating MCP tools (create/update/merge/etc.).
   * Always combined with protectedPathRules() below, regardless of policy. */
  disallowedTools?: string[];
  /** Extra paths the Bash sandbox may write to, beyond `cwd` (which the
   * sandbox always allows). resolve-conflict passes the worktree's git
   * dir here (`git rev-parse --absolute-git-dir`, run inside the
   * worktree) — git keeps a worktree's index/HEAD in the MAIN repo's
   * `.git/worktrees/<id>`, outside the worktree's own `cwd`, so without
   * this `git add`/`git commit` inside the worktree would be denied.
   * Only meaningful for a sandboxed policy (see
   * core/claude-args.js's isSandboxedPolicy); ignored otherwise. */
  sandboxAllowWrite?: string[];
  /** Extra paths the Bash sandbox may NOT write to, beyond whatever the
   * policy itself adds automatically (`readOnly`'s `denyCwdWrite` — see
   * CLAUDE_POLICIES in core/claude-args.js — adds `cwd` there; this
   * option is for anything beyond that). Only meaningful for a sandboxed
   * policy. */
  sandboxDenyWrite?: string[];
  /** Confines the Edit/Write/MultiEdit/NotebookEdit tools to this one
   * directory (real path — resolve symlinks before passing it), via
   * core/edit-guard.js as a PreToolUse hook. Neither the Bash sandbox's
   * `filesystem` rules nor plain `--disallowedTools` permission-rule
   * strings can do this safely on their own: the sandbox never mediates
   * Edit/Write at all (live-verified), and a broad enough
   * `--disallowedTools` pattern denies paths INSIDE the intended root too
   * once that root lives under `$HOME` — exactly the regression that
   * replaced that approach with this hook. resolve-conflict passes the
   * worktree's own real path here. Required by both worktree policies:
   * the same root also confines Read/Glob/Grep (core/read-guard.js). */
  editRoot?: string;
  /** Aborting kills the CLI and rejects (see core/exec.ts's run). */
  signal?: AbortSignal;
  /** Passed through to core/exec.ts's run: called with the CLI's pid (its
   * process-group id, when `signal` is given) as soon as it spawns. The
   * worktree features record it so a restarted service can refuse to touch
   * a worktree an orphaned claude may still be writing. */
  onSpawn?: (pid: number) => void;
  /** Called with a short, human-readable label ("Reading src/a.ts") as Claude
   * works — at most about once a second. Providing it switches the run to
   * stream-json output. */
  onProgress?: (label: string) => void;
}

export interface ClaudeRunResult {
  /** Claude's final text response. */
  text: string;
  /** Raw stdout, kept for debugging when --output-format json didn't parse
   * as expected. */
  raw: string;
  /** `claude`'s own session id for this run (`parsed.session_id`), falling
   * back to `opts.sessionId` when the CLI didn't echo one back (e.g.
   * --output-format json failed to parse) so a caller that supplied its
   * own id can still resume by it. */
  sessionId?: string;
  /** Names of tools the CLI refused to run (from its permission_denials).
   * Only set when `onProgress` is used (stream-json runs); non-streaming
   * runs leave it undefined. */
  permissionDenials?: string[];
}

/**
 * Run `claude` headlessly and non-interactively.
 *
 * `--permission-mode bypassPermissions` is NOT sufficient on its own to
 * skip tool-permission prompts for Edit/Write/Bash — confirmed empirically
 * (this was a real bug: conflicts were silently left unresolved because
 * every Edit call was denied and the failure was only visible in Claude's
 * own JSON `permission_denials` field, not in any server log). The tool
 * also has to be explicitly granted via `--allowedTools`; `--tools` alone
 * only controls which tools are *available*, not which are *permitted*.
 * Both are passed with the same list below.
 *
 * This is still the trust boundary called out in the plan: it is only ever
 * invoked with `cwd` pointing at a disposable worktree that is never
 * pushed until a human approves the resulting diff in the extension panel.
 */
export async function runClaude(opts: ClaudeRunOptions): Promise<ClaudeRunResult> {
  // Neither given is very likely a caller bug (an empty tool grant that was
  // meant to be `policy: "noTools"`, or a typo dropping `tools`) rather than
  // an intentional "no tools at all" — that intent has its own explicit
  // name (`policy: "noTools"`), so silently running with an empty toolset
  // here would hide the mistake instead of surfacing it.
  if (!opts.policy && !opts.tools) {
    throw new Error(
      'runClaude requires either "policy" (a CLAUDE_POLICIES name, e.g. "readOnly") or an ' +
        'explicit "tools" list — neither was given.',
    );
  }

  // `policy` supplies `tools` when given; a bare `tools` list (no policy)
  // behaves exactly as before this option existed — no MCP guard.
  const policy: ClaudePolicy = opts.policy
    ? claudeArgs.CLAUDE_POLICIES[opts.policy]
    : { tools: opts.tools || [], mcpGuard: false };

  const guardScriptPath = path.join(__dirname, "mcp-guard.js");
  if (policy.mcpGuard && !fs.existsSync(guardScriptPath)) {
    // Fail before spawning `claude` at all: a missing guard script under a
    // policy that requires it is exactly the fail-open scenario the guard
    // exists to prevent (see mcp-guard.js's own header) — better a loud,
    // actionable error here than a `--settings` hook command pointing at a
    // file that doesn't exist, which Claude would (per that same fail-open
    // behavior) treat as "no hook", not "blocked".
    throw new Error(`the MCP guard script is missing from ${guardScriptPath}; run npm run build`);
  }

  const editGuardScriptPath = path.join(__dirname, "edit-guard.js");
  if (opts.editRoot && !fs.existsSync(editGuardScriptPath)) {
    // Same fail-before-spawning rationale as the MCP guard check above —
    // a missing edit-guard script when `editRoot` was asked for is the
    // exact fail-open scenario this hook exists to prevent.
    throw new Error(`the edit guard script is missing from ${editGuardScriptPath}; run npm run build`);
  }

  const readGuardScriptPath = path.join(__dirname, "read-guard.js");
  if (policy.readGuard && !fs.existsSync(readGuardScriptPath)) {
    // Same fail-before-spawning rationale: a worktree policy whose read
    // guard is missing would let Claude's Read/Glob/Grep roam the whole
    // disk (see core/read-guard.js).
    throw new Error(`the read guard script is missing from ${readGuardScriptPath}; run npm run build`);
  }

  const bashGuardScriptPath = path.join(__dirname, "bash-guard.js");
  if (policy.bashGuard && !fs.existsSync(bashGuardScriptPath)) {
    // Same fail-before-spawning rationale as the two guards above: without
    // this hook a narrowed Bash grant restricts nothing at all (see
    // core/claude-args.js's worktreeWriteNarrowBash).
    throw new Error(`the bash guard script is missing from ${bashGuardScriptPath}; run npm run build`);
  }

  const pathOpts = {
    stateDir: paths.stateDir(),
    companionDir: claudeArgs.companionServiceDir(),
    home: os.homedir(),
  };
  const protectedRules = claudeArgs.protectedPathRules(pathOpts);
  const sandboxDenyRead = claudeArgs.protectedSandboxPaths(pathOpts);

  // `cwd` is passed through so buildClaudeArgs can add it to
  // sandboxDenyWrite itself for any policy with `denyCwdWrite: true`
  // (readOnly) — no policy-name string check here; a policy asks for
  // this by setting the flag on itself in CLAUDE_POLICIES.
  const args = claudeArgs.buildClaudeArgs({
    prompt: opts.prompt,
    model: opts.model,
    policy,
    sessionId: opts.sessionId,
    cwd: opts.cwd,
    extraAllowedTools: opts.extraAllowedTools,
    disallowedTools: opts.disallowedTools,
    guardScriptPath,
    protectedRules,
    sandboxDenyRead,
    sandboxAllowWrite: opts.sandboxAllowWrite,
    sandboxDenyWrite: opts.sandboxDenyWrite,
    editRoot: opts.editRoot,
    editGuardScriptPath,
    readGuardScriptPath,
    bashGuardScriptPath,
    outputFormat: opts.onProgress ? "stream-json" : undefined,
  });

  const onProgress = opts.onProgress;
  const streamAcc = onProgress ? claudeStream.createStreamAccumulator({ cwd: opts.cwd }) : undefined;
  const throttle = claudeStream.createThrottle(PROGRESS_MIN_INTERVAL_MS);

  const result = await run("claude", args, {
    cwd: opts.cwd,
    signal: opts.signal,
    onSpawn: opts.onSpawn,
    onStdoutLine:
      streamAcc && onProgress
        ? (line) => {
            const { label } = streamAcc.feed(line);
            if (label) throttle(() => onProgress(label));
          }
        : undefined,
  });

  if (streamAcc) {
    const final = streamAcc.result();
    if (final.sawResult) {
      if (final.permissionDenials.length > 0) {
        console.warn(
          `[claude] ${final.permissionDenials.length} tool call(s) were denied:`,
          JSON.stringify(final.permissionDenials),
        );
      }
      return {
        text: final.text || result.stdout,
        raw: result.stdout,
        sessionId: final.sessionId ?? opts.sessionId,
        permissionDenials: final.permissionDenials,
      };
    }
    // No result event (an older CLI, or a cut-off run): fall through to the
    // one-object parse below, which degrades to the raw text.
  }

  try {
    const parsed = JSON.parse(result.stdout);
    // `claude --output-format json` returns a single result object whose
    // final-answer field has varied across CLI versions ("result" is the
    // current one) — fall back to the raw stdout if the shape changes so a
    // CLI upgrade degrades gracefully instead of throwing.
    const text: string =
      typeof parsed.result === "string"
        ? parsed.result
        : typeof parsed.text === "string"
          ? parsed.text
          : result.stdout;
    if (Array.isArray(parsed.permission_denials) && parsed.permission_denials.length > 0) {
      // Not necessarily fatal (e.g. a denied "run the test suite" Bash call
      // is fine, a denied Edit on a conflicted file is not) — the caller
      // decides that by checking whether conflict markers are actually
      // gone. Logged here so it's visible without re-running by hand.
      console.warn(
        `[claude] ${parsed.permission_denials.length} tool call(s) were denied:`,
        JSON.stringify(parsed.permission_denials.map((d: { tool_name?: string }) => d.tool_name)),
      );
    }
    const sessionId: string | undefined =
      typeof parsed.session_id === "string" ? parsed.session_id : opts.sessionId;
    return { text, raw: result.stdout, sessionId };
  } catch {
    return { text: result.stdout, raw: result.stdout, sessionId: opts.sessionId };
  }
}
