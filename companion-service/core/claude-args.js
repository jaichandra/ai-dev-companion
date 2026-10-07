// Pure argv/policy construction for headless `claude` runs — split out of
// core/claude.ts so it can be unit-tested with zero build step, the same
// reason core/paths.js and features/*/plan.js are plain JS (see paths.js's
// header comment). core/claude.ts requires this directly (require + a
// hand-written type cast) and is the only caller in production; nothing
// here touches the filesystem beyond resolving companionServiceDir(), and
// nothing here spawns a process.
const path = require("path");
const { SHELL_METACHARACTERS, prefixesFromBashRules } = require("./bash-command-policy.js");

/**
 * Named tool grants a feature asks for by name instead of hand-listing
 * `tools` on every runClaude call. `mcpGuard: true` means the caller also
 * wires in core/mcp-guard.js as a PreToolUse hook (see buildClaudeArgs)
 * so even an MCP server Claude discovers via ~/.claude.json can't run a
 * mutating tool under this policy — not just the ones a feature's plan.js
 * MUTATING_MCP_TOOLS happens to name.
 */
const CLAUDE_POLICIES = {
  // Analysis / read-only jobs: no Edit/Write/Bash(*) at all, and MCP calls
  // are filtered down to read-only verbs by the guard hook.
  readOnly: {
    tools: [
      "Read",
      "Glob",
      "Grep",
      "WebFetch",
      "WebSearch",
      "Bash(git log:*)",
      "Bash(git blame:*)",
      "Bash(git show:*)",
    ],
    mcpGuard: true,
    // Its Bash grant (`git log`/`blame`/`show`) has no legitimate reason
    // to write anywhere, including its own cwd (e.g. `git log
    // --output=<file>`) — buildClaudeArgs adds `cwd` to the sandbox's
    // `filesystem.denyWrite` for any policy with this set.
    denyCwdWrite: true,
  },
  // Background runs a watcher starts with no click (core/watcher-runner.ts):
  // readOnly minus WebFetch/WebSearch. They work from ticket and PR text
  // anyone can write, so they get no way to send anything out.
  readOnlyBackground: {
    tools: ["Read", "Glob", "Grep", "Bash(git log:*)", "Bash(git blame:*)", "Bash(git show:*)"],
    mcpGuard: true,
    denyCwdWrite: true,
  },
  // resolve-conflict: Claude edits files inside a disposable worktree (see
  // core/worktree.ts) that's never pushed until a human approves the diff.
  // No MCP guard — that job doesn't grant any MCP tools in the first place.
  //
  // `readGuard: true` (both worktree policies) wires core/read-guard.js in
  // as a PreToolUse hook confining Read/Glob/Grep to the same root as the
  // edit guard (`editRoot`) — the sandbox's denyRead only covers Bash and
  // the files protectedPathRules names, and the PR being worked on is
  // untrusted input that may ask Claude to go reading elsewhere.
  worktreeWrite: {
    tools: ["Read", "Edit", "Write", "Bash", "Glob", "Grep"],
    mcpGuard: false,
    readGuard: true,
  },
  // address-review-comments: the same worktree confinement as
  // worktreeWrite (sandboxed Bash, and the caller passes editRoot for the
  // edit guard), but Bash is narrowed to looking at its own changes —
  // `git diff`/`git status` — plus whatever check commands the user
  // configured (appended per run via checkCommandRules + extraAllowedTools,
  // since they come from config, not from this static table). Addressing
  // a review comment is an edit, not a build or a commit.
  //
  // `bashGuard: true` is what ENFORCES that: the `Bash(...)` entries only
  // pre-approve matching calls — under bypassPermissions they restrict
  // nothing (live-verified: `touch x && echo MADE` ran with no denial) —
  // so buildClaudeArgs wires core/bash-guard.js in as a PreToolUse hook
  // that denies any Bash call not matching one of those same rules.
  worktreeWriteNarrowBash: {
    tools: ["Read", "Edit", "Write", "Glob", "Grep", "Bash(git diff:*)", "Bash(git status:*)"],
    mcpGuard: false,
    readGuard: true,
    bashGuard: true,
  },
  // A Claude call with nothing to do but answer in text (e.g. analyze-issue's
  // repo-pick prompt, which explicitly tells Claude not to use tools).
  noTools: {
    tools: [],
    mcpGuard: false,
  },
};

/** `Bash(git log:*)` -> `Bash`; bare names pass through. `--tools` only
 * accepts built-in tool names, not permission patterns. Moved here from
 * core/claude.ts along with the rest of argv construction; behavior is
 * unchanged. */
function baseToolName(tool) {
  const match = tool.match(/^([A-Za-z]+)\(/);
  return match ? match[1] : tool;
}

/**
 * Single-quote shell-escapes `value` for embedding in the guard hook's
 * `command` string, which Claude hands to the user's shell to execute.
 * Wrapping a value in single quotes disables ALL shell interpretation —
 * `$VAR`/`$(...)` expansion, globbing, backticks, everything — except for
 * a literal single quote itself, which has to close the quoting, emit an
 * escaped literal quote, and reopen it: `'` -> `'\''`. Double-quoting
 * (`"${value}"`) is NOT enough here: double quotes still expand `$...`,
 * which is exactly the kind of guard-script path this hook must survive
 * (an install path containing a `$`, a space, or an apostrophe should
 * neither break the hook nor get silently mis-parsed).
 */
function shellQuoteSingle(value) {
  return "'" + String(value).replace(/'/g, "'\\''") + "'";
}

/**
 * companion-service/ root — works whether this file is loaded from core/
 * (source / node test) or dist/core/ (after tsc). Same resolution as
 * features/analyze-issue/plan.js's companionServiceDir(), just one
 * directory shallower (core/ vs features/analyze-issue/); kept as its own
 * small copy rather than requiring a features/ module from core/.
 */
function companionServiceDir() {
  let dir = path.join(__dirname, "..");
  if (path.basename(dir) === "dist") dir = path.join(dir, "..");
  return dir;
}

/**
 * The common places a developer's own credentials live, relative to
 * `$HOME` — denied to EVERY Claude run (as `Read(...)` rules and as sandbox
 * `denyRead` paths), not just the worktree ones: an analysis job reads
 * untrusted PR/issue text too, and none of these is ever something a job
 * needs. Directories (denied with everything under them) and single files
 * are listed apart because the `Read(...)` rule for a directory needs a
 * trailing `/**`. `.bitbucket-ai-companion` is the companion's own legacy
 * state dir (pre-migration config and token).
 */
const SECRET_HOME_DIRS = [".ssh", ".aws", ".gnupg", ".config/gh", ".kube", ".bitbucket-ai-companion"];
const SECRET_HOME_FILES = [".netrc", ".npmrc", ".docker/config.json", ".cursor/mcp.json"];

/**
 * Deny rules passed to `--disallowedTools`, using Claude's `//<absolute
 * path>` syntax. Deliberately narrow: credentials.key/credentials.enc/
 * config.json/.claude.json/history.db (and its -wal/-shm) are protected individually, and Edit is denied
 * across the whole companion-service tree (Claude should never rewrite its
 * own code), but stateDir itself is NOT blanket-denied — resolve-conflict's
 * disposable worktrees live under it (core/worktree.ts's WORKTREE_ROOT),
 * and Claude needs to Read/Edit/Write inside those for that feature to work
 * at all.
 */
function protectedPathRules({ stateDir, companionDir, home }) {
  // Claude's absolute-path syntax is `//<absolute path>` — stateDir/
  // companionDir/home are themselves absolute (leading "/"), so a single
  // extra "/" prefix is all that's needed here, not "//".
  return [
    `Read(/${path.join(stateDir, "credentials.key")})`,
    `Read(/${path.join(stateDir, "history.db")})`,
    `Read(/${path.join(stateDir, "history.db-wal")})`,
    `Read(/${path.join(stateDir, "history.db-shm")})`,
    `Read(/${path.join(companionDir, "credentials.enc")})`,
    `Read(/${path.join(companionDir, "config.json")})`,
    `Edit(/${path.join(companionDir, "**")})`,
    `Read(/${path.join(home, ".claude.json")})`,
    ...SECRET_HOME_DIRS.map((dir) => `Read(/${path.join(home, dir, "**")})`),
    ...SECRET_HOME_FILES.map((file) => `Read(/${path.join(home, file)})`),
  ];
}

/**
 * The same protected files as protectedPathRules(), but as plain absolute
 * paths (no leading `//`, no Read()/Edit() wrapper) — the form the Claude
 * Code sandbox's `filesystem.denyRead` expects (see the "Claude Code
 * sandbox facts" in the SDD constraints: sandbox paths use normal `/abs`
 * or `~/` paths, unlike the `//abs` form `--disallowedTools` rules use).
 * Deliberately the individual files, not `stateDir` as a whole — same
 * reasoning as protectedPathRules: resolve-conflict's disposable worktrees
 * live under stateDir, and Claude needs to read/write inside those.
 */
function protectedSandboxPaths({ stateDir, companionDir, home }) {
  return [
    path.join(stateDir, "credentials.key"),
    path.join(stateDir, "history.db"),
    path.join(stateDir, "history.db-wal"),
    path.join(stateDir, "history.db-shm"),
    path.join(companionDir, "credentials.enc"),
    path.join(companionDir, "config.json"),
    path.join(home, ".claude.json"),
    ...SECRET_HOME_DIRS.map((dir) => path.join(home, dir)),
    ...SECRET_HOME_FILES.map((file) => path.join(home, file)),
  ];
}

/**
 * Whether `tool` is `Bash` itself or a scoped permission pattern like
 * `Bash(git log:*)` — either way, the policy grants some form of shell
 * execution and must run under the sandbox (see isSandboxedPolicy).
 */
function grantsBash(tool) {
  return tool === "Bash" || tool.startsWith("Bash(");
}

/**
 * A policy needs the Bash sandbox (Claude Code's `sandbox.enabled`
 * setting) exactly when it grants any form of Bash: `readOnly` (scoped to
 * `git log`/`blame`/`show`) and `worktreeWrite` (unscoped `Bash`) both do;
 * `noTools` grants no tools at all, so there is no Bash call to sandbox.
 * Read/Edit `--disallowedTools` rules (protectedPathRules) never governed
 * Bash — this is what closes that gap.
 */
function isSandboxedPolicy(policy) {
  return policy.tools.some(grantsBash);
}

/**
 * Builds the `sandbox` object for `--settings`. Pure and independent of
 * any policy — buildClaudeArgs decides *whether* to include this via
 * isSandboxedPolicy(); this just shapes the filesystem rules once it does.
 * `denyRead` is always present (protectedPathRules's files are always
 * denied to a sandboxed run); `allowWrite`/`denyWrite` are per-run and
 * omitted entirely when empty rather than sent as `[]`, so a run that
 * doesn't need either doesn't have to reason about what an empty array
 * means to the sandbox.
 */
function sandboxSettings({ denyRead = [], allowWrite = [], denyWrite = [] } = {}) {
  const filesystem = { denyRead };
  if (allowWrite.length > 0) filesystem.allowWrite = allowWrite;
  if (denyWrite.length > 0) filesystem.denyWrite = denyWrite;
  return {
    enabled: true,
    // A command the sandbox can't mediate (e.g. one Seatbelt/bubblewrap
    // doesn't support) must fail loudly, not silently run unsandboxed —
    // that would defeat the entire point of this setting.
    allowUnsandboxedCommands: false,
    // If the sandbox mechanism itself can't start (missing bwrap/socat on
    // Linux, say — see doctor.js), the run must fail rather than silently
    // fall back to running Bash unsandboxed.
    failIfUnavailable: true,
    filesystem,
  };
}

/**
 * Builds the `claude` CLI argv. Pulled out of runClaude (core/claude.ts) so
 * the exact flag order/semantics — verified empirically, see that file's
 * runClaude doc comment — can be tested here with no process spawned.
 *
 * `policy` is one of CLAUDE_POLICIES's values (not a key): it supplies both
 * the base `tools` list and whether the MCP guard hook is wired in.
 *
 * `execPath` defaults to `process.execPath` (the node binary actually
 * running this process) rather than a bare `node` — the hook command must
 * not depend on `node` being first on the invoking shell's PATH (it may
 * not be, e.g. under launchd). Overridable for tests.
 */
function buildClaudeArgs({
  prompt,
  model,
  policy,
  sessionId,
  cwd,
  extraAllowedTools = [],
  disallowedTools = [],
  guardScriptPath,
  protectedRules = [],
  sandboxDenyRead = [],
  sandboxAllowWrite = [],
  sandboxDenyWrite = [],
  editRoot,
  editGuardScriptPath,
  readGuardScriptPath,
  bashGuardScriptPath,
  outputFormat = "json",
  execPath = process.execPath,
}) {
  const allowedTools = [...policy.tools, ...extraAllowedTools];
  const availabilityTools = [...new Set(policy.tools.map(baseToolName))];

  const args = [
    "-p",
    prompt,
    // No --model unless a feature set one: Claude Code then uses its own default.
    ...(model ? ["--model", model] : []),
    "--permission-mode",
    "bypassPermissions",
    "--allowedTools",
    allowedTools.join(" "),
    "--output-format",
    outputFormat,
    "--tools",
    availabilityTools.join(","),
  ];

  // The CLI requires --verbose alongside stream-json.
  if (outputFormat === "stream-json") args.push("--verbose");

  if (sessionId) {
    args.push("--session-id", sessionId);
  }

  const allDisallowed = [...disallowedTools, ...protectedRules];
  if (allDisallowed.length > 0) {
    args.push("--disallowedTools", allDisallowed.join(" "));
  }

  // The MCP guard hook (readOnly only), the edit-confinement guard
  // (via `editRoot`), the read guard and the bash guard (per policy flag),
  // and the Bash sandbox (any policy that grants Bash) are all merged into
  // ONE `--settings` object — Claude Code only accepts a single
  // `--settings` flag per invocation. The hooks share one `PreToolUse`
  // array; Claude Code runs every entry whose matcher fits the call.
  const settings = {};
  const preToolUseHooks = [];

  if (policy.mcpGuard) {
    // Single-quoted, not `"${...}"`: this string is handed to the user's
    // shell verbatim, and a guard-script or node path containing a space,
    // `$`, or `'` must survive that unmangled (see shellQuoteSingle).
    const command = `${shellQuoteSingle(execPath)} ${shellQuoteSingle(guardScriptPath)}`;
    preToolUseHooks.push({
      matcher: "mcp__.*",
      hooks: [{ type: "command", command }],
    });
  }

  if (editRoot) {
    // core/edit-guard.js — fail-closed filesystem confinement for
    // Edit/Write/MultiEdit/NotebookEdit to `editRoot`. Takes the allowed
    // root as a THIRD shell-quoted argv entry (after the node binary and
    // the script path itself), same quoting rationale as the MCP guard's
    // command above.
    const command = `${shellQuoteSingle(execPath)} ${shellQuoteSingle(editGuardScriptPath)} ${shellQuoteSingle(editRoot)}`;
    preToolUseHooks.push({
      matcher: "Edit|Write|MultiEdit|NotebookEdit",
      hooks: [{ type: "command", command }],
    });
  }

  if (policy.readGuard) {
    // core/read-guard.js — fail-closed confinement of Read/Glob/Grep to the
    // same root as the edit guard. A worktree policy with no root (or no
    // script) would silently leave Claude's reads unconfined, so refuse
    // instead, same as the bash guard below.
    if (!editRoot || !readGuardScriptPath) {
      throw new Error("this policy requires the read guard, but no editRoot or readGuardScriptPath was given");
    }
    const command = `${shellQuoteSingle(execPath)} ${shellQuoteSingle(readGuardScriptPath)} ${shellQuoteSingle(editRoot)}`;
    preToolUseHooks.push({
      matcher: "Read|Glob|Grep",
      hooks: [{ type: "command", command }],
    });
  }

  if (policy.bashGuard) {
    // core/bash-guard.js — the enforcement for a narrowed Bash grant (see
    // worktreeWriteNarrowBash). Its allowlist is derived from the very
    // `Bash(<prefix>:*)` rules --allowedTools pre-approves, so the two can't
    // disagree; passed as one shell-quoted JSON argv entry. No script path
    // is a caller bug, and running unguarded would silently widen Bash back
    // to anything — so refuse instead.
    if (!bashGuardScriptPath) {
      throw new Error("this policy requires the bash guard, but no bashGuardScriptPath was given");
    }
    const prefixes = prefixesFromBashRules(allowedTools);
    const command = `${shellQuoteSingle(execPath)} ${shellQuoteSingle(bashGuardScriptPath)} ${shellQuoteSingle(JSON.stringify(prefixes))}`;
    preToolUseHooks.push({
      matcher: "Bash",
      hooks: [{ type: "command", command }],
    });
  }

  if (preToolUseHooks.length > 0) {
    settings.hooks = { PreToolUse: preToolUseHooks };
  }

  if (isSandboxedPolicy(policy)) {
    // policy.denyCwdWrite (readOnly) means "this policy's Bash grant has
    // no legitimate reason to write, including into its own cwd" — added
    // here rather than by every caller, so a policy that wants it gets it
    // automatically instead of every call site remembering to ask.
    const denyWrite = policy.denyCwdWrite && cwd ? [...sandboxDenyWrite, cwd] : sandboxDenyWrite;
    settings.sandbox = sandboxSettings({
      denyRead: sandboxDenyRead,
      allowWrite: sandboxAllowWrite,
      denyWrite,
    });
  }

  if (Object.keys(settings).length > 0) {
    args.push("--settings", JSON.stringify(settings));
  }

  return args;
}

/**
 * `addressReviewComments.checkCommands` from config.json, trimmed and
 * validated — the one list the prompt, the `--allowedTools` rules and the
 * bash guard all use. Each entry becomes part of a permission-rule string,
 * so anything that could close the rule early or widen it is refused
 * outright rather than escaped: `(`/`)` delimit the rule, `*` is its
 * wildcard, and a newline would split one rule into two. Anything the
 * bash guard would refuse at run time (core/bash-command-policy.js's
 * SHELL_METACHARACTERS: chaining, pipes, `$`, redirection, backslashes) is
 * refused here too, so a configured check can never be one Claude isn't
 * allowed to run. Throws with the offending index so the user knows which
 * entry in config.json to fix.
 */
function normalizeCheckCommands(checkCommands) {
  if (checkCommands === undefined || checkCommands === null) return [];
  if (!Array.isArray(checkCommands)) {
    throw new Error(
      "addressReviewComments.checkCommands in config.json must be an array of command strings, " +
        'e.g. ["npm test", "npx tsc --noEmit"].',
    );
  }
  return checkCommands.map((raw, i) => {
    const cmd = typeof raw === "string" ? raw.trim() : "";
    if (!cmd || cmd.includes("*") || SHELL_METACHARACTERS.test(cmd)) {
      throw new Error(
        `addressReviewComments.checkCommands[${i}] in config.json (${JSON.stringify(raw)}) must be one ` +
          "plain non-empty command with no ; & | ` $ ( ) < > \\ * or line breaks — fix or remove it, then retry.",
      );
    }
    return cmd;
  });
}

/** normalizeCheckCommands' list -> one `Bash(<cmd>:*)` allow rule each, for
 * worktreeWriteNarrowBash's extraAllowedTools (and so its bash guard). */
function checkCommandRules(checkCommands) {
  return normalizeCheckCommands(checkCommands).map((cmd) => `Bash(${cmd}:*)`);
}

module.exports = {
  CLAUDE_POLICIES,
  normalizeCheckCommands,
  checkCommandRules,
  protectedPathRules,
  protectedSandboxPaths,
  isSandboxedPolicy,
  sandboxSettings,
  buildClaudeArgs,
  companionServiceDir,
};
