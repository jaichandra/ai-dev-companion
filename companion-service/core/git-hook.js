// The optional pre-push hook, set in a repo's own git config (git's
// config-based hooks) so it runs alongside husky and any existing pre-push
// hook and touches no file:
//   git config --local hook.companion-precheck.event pre-push
//   git config --local hook.companion-precheck.command "companion precheck --stdin"
// Pure: argv lists for `git` (always an argv array, never a shell string)
// and the parsing of what git hands a pre-push hook on stdin.
const HOOK_NAME = "companion-precheck";
const HOOK_EVENT = "pre-push";
const HOOK_COMMAND = "companion precheck --stdin";
const PROBE_MARKER = "companion-probe-ok";
const MAX_UPDATES = 50;
const SHA_RE = /^([0-9a-f]{40}|[0-9a-f]{64})$/;
const ZERO_RE = /^0+$/;

function assertRepoDir(repo) {
  if (typeof repo !== "string" || !repo.startsWith("/") || /[\u0000-\u001f]/.test(repo)) {
    throw new Error("The repository must be an absolute folder path.");
  }
}

/** Two `git` argv lists that add the hook to `repo`'s local config. */
function installArgs(repo) {
  assertRepoDir(repo);
  return [
    ["-C", repo, "config", "--local", `hook.${HOOK_NAME}.event`, HOOK_EVENT],
    ["-C", repo, "config", "--local", `hook.${HOOK_NAME}.command`, HOOK_COMMAND],
  ];
}

/** `git config --unset` for both keys (exit 5 just means "wasn't set"). */
function uninstallArgs(repo) {
  assertRepoDir(repo);
  return [
    ["-C", repo, "config", "--local", "--unset-all", `hook.${HOOK_NAME}.event`],
    ["-C", repo, "config", "--local", "--unset-all", `hook.${HOOK_NAME}.command`],
  ];
}

function statusArgs(repo) {
  assertRepoDir(repo);
  return ["-C", repo, "config", "--local", "--get", `hook.${HOOK_NAME}.command`];
}

/** Steps that show whether this git runs config-based hooks: a throwaway
 * repo in `dir` with a probe hook, then `git hook run`. Supported when the
 * last step prints PROBE_MARKER. */
function probeSteps(dir) {
  assertRepoDir(dir);
  return [
    ["init", "-q", "--template=", dir],
    ["-C", dir, "config", "--local", "hook.companion-probe.event", HOOK_EVENT],
    ["-C", dir, "config", "--local", "hook.companion-probe.command", `echo ${PROBE_MARKER}`],
    ["-C", dir, "hook", "run", HOOK_EVENT],
  ];
}

/** What to tell someone whose git has no config-based hooks. */
function fallbackInstructions(repo) {
  return (
    `This git doesn't run hooks from its config. To get the check anyway, add this line to ${repo}/.git/hooks/pre-push ` +
    "(or to husky's .husky/pre-push) and make the file executable:\n\n    companion precheck --stdin || true\n"
  );
}

/** The `<local ref> <local sha> <remote ref> <remote sha>` lines git feeds
 * a pre-push hook. Malformed lines are skipped; at most 50. */
function parsePrePushLines(text) {
  const out = [];
  for (const line of String(text || "").split("\n")) {
    const parts = line.trim().split(/\s+/);
    if (parts.length !== 4 || !SHA_RE.test(parts[1]) || !SHA_RE.test(parts[3])) continue;
    out.push({ localRef: parts[0], localSha: parts[1], remoteRef: parts[2], remoteSha: parts[3] });
    if (out.length >= MAX_UPDATES) break;
  }
  return out;
}

/** The `git` argv that lists one update's changed files, or null for a
 * deleted branch. A new branch lists files of commits no remote branch has. */
function changedFilesArgs(update) {
  // Only real object ids reach git, and a trailing `--` means nothing after
  // them can ever be read as a path or an option.
  if (!update || !SHA_RE.test(update.localSha) || !SHA_RE.test(update.remoteSha)) return null;
  if (ZERO_RE.test(update.localSha)) return null;
  if (ZERO_RE.test(update.remoteSha)) return ["log", "--name-only", "--format=", update.localSha, "--not", "--remotes", "--"];
  return ["diff", "--name-only", update.remoteSha, update.localSha, "--"];
}

/** `git diff/log --name-only` output -> unique non-empty paths. */
function parseNameList(stdout) {
  return [...new Set(String(stdout || "").split("\n").map((l) => l.trim()).filter(Boolean))];
}

/** The environment for a `git` call: prompts off, file names never
 * C-quoted (core.quotePath=false, via GIT_CONFIG_COUNT so the argv stays
 * as is). `isolated` (the probe) also ignores the user's and the system's
 * git config and any config injected through the environment. */
function gitEnv(base, { isolated = false } = {}) {
  const env = { ...base, GIT_TERMINAL_PROMPT: "0" };
  let n = 0;
  if (!isolated) {
    const given = Number.parseInt(env.GIT_CONFIG_COUNT, 10);
    n = Number.isInteger(given) && given > 0 && given < 1000 ? given : 0;
  } else {
    delete env.GIT_CONFIG_COUNT;
    env.GIT_CONFIG_GLOBAL = "/dev/null";
    env.GIT_CONFIG_NOSYSTEM = "1";
  }
  env[`GIT_CONFIG_KEY_${n}`] = "core.quotePath";
  env[`GIT_CONFIG_VALUE_${n}`] = "false";
  env.GIT_CONFIG_COUNT = String(n + 1);
  return env;
}

module.exports = {
  gitEnv,
  HOOK_NAME,
  HOOK_EVENT,
  HOOK_COMMAND,
  PROBE_MARKER,
  installArgs,
  uninstallArgs,
  statusArgs,
  probeSteps,
  fallbackInstructions,
  parsePrePushLines,
  changedFilesArgs,
  parseNameList,
};
