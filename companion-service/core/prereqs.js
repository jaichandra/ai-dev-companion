// Prerequisite checks shared by setup.js (interactive wizard) and
// server.ts (fail-fast startup check). Plain JS, not TypeScript, so
// setup.js can run this directly with zero build step on a fresh clone —
// tsc (with allowJs: true) still picks it up and copies it into dist/ for
// server.ts's runtime use after a build.
const { execFileSync } = require("child_process");
const fs = require("fs");
const net = require("net");
const os = require("os");
const path = require("path");

// Node's built-in SQLite (the local history store) is unflagged from 22.13.
const MIN_NODE = { major: 22, minor: 13 };
const MIN_NODE_MAJOR = MIN_NODE.major;

function nodeMeetsMinimum(version) {
  const [major, minor] = String(version).split(".").map((n) => parseInt(n, 10));
  if (!Number.isFinite(major) || !Number.isFinite(minor)) return false;
  return major > MIN_NODE.major || (major === MIN_NODE.major && minor >= MIN_NODE.minor);
}

function checkNodeVersion(version = process.versions.node) {
  if (!nodeMeetsMinimum(version)) {
    return {
      ok: false,
      message:
        `Node ${version} found; need Node ${MIN_NODE.major}.${MIN_NODE.minor}+ ` +
        "(the local history uses Node's built-in SQLite). Install a newer Node from https://nodejs.org.",
    };
  }
  return { ok: true, message: `Node ${version} OK.` };
}

function checkGit() {
  try {
    const out = execFileSync("git", ["--version"], { encoding: "utf8" }).trim();
    return { ok: true, message: `${out} OK.` };
  } catch {
    return {
      ok: false,
      message: "git not found on PATH. Install git (on macOS: `xcode-select --install`) and try again.",
    };
  }
}

function checkClaudeCli() {
  try {
    const out = execFileSync("claude", ["--version"], { encoding: "utf8" }).trim();
    return { ok: true, message: `claude CLI ${out} found.` };
  } catch {
    return {
      ok: false,
      message: "claude CLI not found on PATH. Install Claude Code first: https://claude.com/claude-code",
    };
  }
}

// A 5s timeout with SIGKILL:
// a hung `claude` process (locked keychain, stuck network prompt) must not
// block this check forever — GET /settings (runAllChecks) and server.ts's
// startup/assertFeatureReady checks all call this synchronously, so a wedged
// child process here would wedge the whole event loop behind it. SIGKILL,
// not the default SIGTERM, because `claude` may be ignoring signals in
// whatever state it's stuck in.
const CLAUDE_AUTH_TIMEOUT_MS = 5000;

/** `timeoutMs` is injectable (defaults to CLAUDE_AUTH_TIMEOUT_MS) purely so
 * prereqs.test.js can exercise the timeout path in well under 5s, without
 * a real `claude` binary; every real caller (CHECKS_BY_NAME, etc.) just
 * uses the default. */
function checkClaudeAuth(timeoutMs = CLAUDE_AUTH_TIMEOUT_MS) {
  let out;
  try {
    out = execFileSync("claude", ["auth", "status"], {
      encoding: "utf8",
      timeout: timeoutMs,
      killSignal: "SIGKILL",
    });
  } catch (err) {
    // `claude auth status` exits non-zero when simply logged out, still
    // printing its normal {"loggedIn": false, ...} JSON to stdout, so a
    // non-zero exit with output is a status to parse rather than an error.
    out = typeof err.stdout === "string" ? err.stdout : "";
    if (!out.trim()) {
      // execFileSync sets err.signal (not a normal exit) when `timeout`
      // fired — worth calling out explicitly rather than folding it into
      // the generic "Could not check" message below.
      if (err.signal) {
        return {
          ok: false,
          message: `claude auth status did not respond within ${timeoutMs / 1000}s — it may be stuck (a locked keychain, a stalled network prompt). Try running it by hand.`,
        };
      }
      return { ok: false, message: `Could not check claude auth status: ${err.message}` };
    }
  }
  try {
    const status = JSON.parse(out);
    if (!status.loggedIn) {
      return { ok: false, message: "claude CLI is installed but not logged in. Run `claude auth login`." };
    }
    return { ok: true, message: `claude CLI logged in as ${status.email || "unknown user"}.` };
  } catch (err) {
    return { ok: false, message: `Could not parse claude auth status output: ${err.message}` };
  }
}

function checkPortFree(port) {
  return new Promise((resolve) => {
    const tester = net.createServer();
    tester.once("error", () => resolve({ ok: false, message: `Port ${port} is already in use.` }));
    tester.once("listening", () => {
      tester.close(() => resolve({ ok: true, message: `Port ${port} is free.` }));
    });
    tester.listen(port, "127.0.0.1");
  });
}

function checkRepoPath(repoPath) {
  if (!fs.existsSync(repoPath)) {
    return { ok: false, message: `${repoPath} does not exist.` };
  }
  if (!fs.existsSync(`${repoPath}/.git`)) {
    return { ok: false, message: `${repoPath} is not a git repository (no .git found).` };
  }
  try {
    execFileSync("git", ["-C", repoPath, "remote", "get-url", "origin"], { encoding: "utf8" });
  } catch {
    return { ok: false, message: `${repoPath} has no "origin" remote configured.` };
  }
  try {
    const shallow = execFileSync("git", ["-C", repoPath, "rev-parse", "--is-shallow-repository"], {
      encoding: "utf8",
    }).trim();
    if (shallow === "true") {
      return {
        ok: false,
        message:
          `${repoPath} is a shallow clone — run "git fetch --unshallow" there first ` +
          `(a shallow clone silently breaks merge-conflict detection).`,
      };
    }
  } catch {
    // If this failed, the origin-remote check above already would have failed first.
  }
  return { ok: true, message: `${repoPath} OK.` };
}

function getOriginUrl(repoPath) {
  try {
    return execFileSync("git", ["-C", repoPath, "remote", "get-url", "origin"], {
      encoding: "utf8",
    }).trim();
  } catch {
    return null;
  }
}

/**
 * True if originUrl looks like it points at exactly this project/repo (a Bitbucket project and repo
 * slug, or a GitHub owner and repo). Bitbucket Server's own clone URLs lowercase the project key in the
 * path (e.g. ".../scm/acme/sample-app.git" for project "ACME"), and GitHub names are case-insensitive, so the
 * comparison is case-insensitive. The ".git" suffix is optional: `git clone https://github.com/o/r`
 * leaves an origin without it.
 */
function originMatchesProjectRepo(originUrl, project, repo) {
  if (!originUrl) return false;
  const escape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const pattern = new RegExp(`[:/]${escape(project)}/${escape(repo)}(?:\\.git)?/?$`, "i");
  return pattern.test(originUrl.trim());
}

/**
 * Parses a Bitbucket Server origin URL into the "PROJECT/repo" key the
 * extension sends (project keys are upper-case in PR URLs, lower-case in
 * clone URLs). Accepts the HTTPS shape (".../scm/acme/sample-app.git") and the
 * SSH shapes ("ssh://git@host:7999/acme/sample-app.git", "git@host:acme/sample-app.git").
 * Returns null for anything else — personal (~user) repos, and GitHub/GitLab
 * remotes, which would otherwise parse as a plausible-looking owner/repo.
 */
function parseBitbucketOrigin(originUrl) {
  if (!originUrl) return null;
  const url = originUrl.trim();
  if (/github\.com|gitlab\.com/i.test(url)) return null;
  const match = /^https?:/i.test(url)
    ? url.match(/\/scm\/([^/]+)\/([^/]+?)(?:\.git)?\/?$/i)
    : url.match(/[:/]([^/:]+)\/([^/]+?)(?:\.git)?\/?$/);
  if (!match || match[1].startsWith("~")) return null;
  return { project: match[1].toUpperCase(), repo: match[2], key: `${match[1].toUpperCase()}/${match[2]}` };
}

/**
 * Parses a GitHub (or GitHub Enterprise) origin URL into its "owner/repo" key. Accepts the HTTPS shape
 * ("https://github.com/o/r.git", with or without ".git"), "git@github.com:o/r.git" and
 * "ssh://git@host[:port]/o/r.git". Returns null for anything else, and for a Bitbucket "/scm/" path.
 */
function parseGithubOrigin(originUrl) {
  if (!originUrl) return null;
  const url = originUrl.trim();
  if (/\/scm\//i.test(url)) return null;
  const match = /^[a-z][a-z0-9+.-]*:\/\//i.test(url)
    ? url.match(/^[a-z][a-z0-9+.-]*:\/\/(?:[^/@]+@)?[^/]+\/([^/]+)\/([^/]+?)(?:\.git)?\/?$/i)
    : url.match(/^(?:[^@/]+@)?[^:/]+:([^/]+)\/([^/]+?)(?:\.git)?\/?$/);
  if (!match) return null;
  return { project: match[1], repo: match[2], key: `${match[1]}/${match[2]}` };
}

/**
 * How each git provider names things: reading a clone's origin, where a fresh clone comes from, and the
 * shape of a pull request's web address and of the key it is filed under in history and scope keys.
 *   prKey        "<host>:<project>/<repo>#<n>" (Bitbucket upper-cases the project, GitHub lower-cases both)
 *   prPath       the path of a PR page (Bitbucket: /projects/P/repos/R/pull-requests/N, GitHub: /o/r/pull/N)
 *   parsePrUrl   { project, repo, id } of a PR page's address, or null
 *   parseRepoUrl { project, repo } of any page of a repository, or null (for addresses in ticket text)
 */
const GIT_REMOTE_RULES = {
  "bitbucket-dc": {
    keyPrefix: "bitbucket",
    parseOrigin: (url) => parseBitbucketOrigin(url),
    cloneUrlTail: (project, repo) => `${project.toLowerCase()}/${repo}.git`,
    pageCloneUrl: (pageOrigin, project, repo) => `${pageOrigin.replace(/\/$/, "")}/scm/${project.toLowerCase()}/${repo}.git`,
    prKey: (project, repo, id) => `bitbucket:${String(project).toUpperCase()}/${String(repo).toLowerCase()}#${id}`,
    prPath: (project, repo, id) => `/projects/${encodeURIComponent(project)}/repos/${encodeURIComponent(repo)}/pull-requests/${Number(id)}`,
    parsePrUrl: (url) => {
      const m = /\/projects\/([^/]+)\/repos\/([^/?#]+)\/pull-requests\/(\d+)/i.exec(String(url));
      return m ? { project: m[1], repo: m[2], id: Number(m[3]) } : null;
    },
    parseRepoUrl: (url) => {
      const m = /\/projects\/([^/]+)\/repos\/([^/?#]+)/i.exec(String(url));
      return m ? { project: m[1], repo: m[2] } : null;
    },
  },
  github: {
    keyPrefix: "github",
    parseOrigin: (url) => parseGithubOrigin(url),
    cloneUrlTail: (project, repo) => `${project}/${repo}.git`,
    pageCloneUrl: (pageOrigin, project, repo) => `${pageOrigin.replace(/\/$/, "")}/${project}/${repo}.git`,
    prKey: (project, repo, id) => `github:${String(project).toLowerCase()}/${String(repo).toLowerCase()}#${id}`,
    prPath: (project, repo, id) => `/${encodeURIComponent(project)}/${encodeURIComponent(repo)}/pull/${Number(id)}`,
    parsePrUrl: (url) => {
      const m = /^(?:[a-z]+:\/\/[^/]+)?\/([^/]+)\/([^/?#]+)\/pull\/(\d+)/i.exec(String(url).replace(/^(https?:\/\/[^/]+)\/api\/v3/, "$1"));
      return m ? { project: m[1], repo: m[2], id: Number(m[3]) } : null;
    },
    parseRepoUrl: (url) => {
      const m = /^(?:[a-z]+:\/\/[^/]+)?\/([^/]+)\/([^/?#]+)/i.exec(String(url));
      return m ? { project: m[1], repo: m[2] } : null;
    },
  },
};

/** The remote rules of this distribution's git provider (environment.js `sites`); Bitbucket's when it names none. */
function gitRemoteRules() {
  const site = require("../environment.js").sites.find((s) => s.kind === "git");
  return GIT_REMOTE_RULES[site && site.provider] || GIT_REMOTE_RULES["bitbucket-dc"];
}

/** Reads `remote "origin"`'s url straight from .git/config — much faster
 * than spawning git once per candidate while scanning whole folders. */
function readOriginFromGitConfig(repoDir) {
  try {
    const text = fs.readFileSync(path.join(repoDir, ".git", "config"), "utf8");
    const section = text.split(/^\s*\[/m).find((s) => /^remote\s+"origin"\s*\]/.test(s));
    const url = section && section.match(/^\s*url\s*=\s*(.+)$/m);
    return url ? url[1].trim() : null;
  } catch {
    return null;
  }
}

/** Folders people commonly keep clones in — searched (one level deep) by
 * setup's repo discovery and by runtime inference for an unmapped repo. */
function defaultSearchRoots() {
  const home = os.homedir();
  return [
    "gitviews",
    "git",
    "src",
    "code",
    "projects",
    "workspace",
    "repos",
    "dev",
    "Development",
    path.join("Documents", "GitHub"),
    path.join("Documents", "git"),
  ]
    .map((dir) => path.join(home, dir))
    .filter((dir) => fs.existsSync(dir));
}

/**
 * Every Bitbucket clone found directly inside `roots` (one level deep),
 * as [{ key, path }] sorted by key. Only real clones count — a directory
 * whose .git is a *file* is a linked worktree of some other clone, and
 * `*.worktrees` folders are this tool's own review worktrees.
 */
function discoverLocalClones(roots) {
  const found = new Map();
  for (const root of new Set(roots)) {
    let names;
    try {
      names = fs.readdirSync(root);
    } catch {
      continue;
    }
    for (const name of names) {
      if (name.startsWith(".") || name.endsWith(".worktrees") || name === "node_modules") continue;
      const dir = path.join(root, name);
      try {
        if (!fs.statSync(path.join(dir, ".git")).isDirectory()) continue;
      } catch {
        continue;
      }
      const parsed = gitRemoteRules().parseOrigin(readOriginFromGitConfig(dir));
      if (parsed && !found.has(parsed.key)) found.set(parsed.key, dir);
    }
  }
  return [...found.entries()]
    .map(([key, dir]) => ({ key, path: dir }))
    .sort((a, b) => a.key.localeCompare(b.key));
}

/** True for a project key / owner / repo name that's safe to use as a path segment and in a clone URL.
 * A single leading dot is allowed (GitHub's ".github" repository), never "." or ".." or a leading dash. */
function isSafeRepoSegment(value) {
  return typeof value === "string" && /^(?:[A-Za-z0-9]|\.[A-Za-z0-9])[A-Za-z0-9._-]*$/.test(value) && !value.includes("..") && value.toLowerCase() !== ".git";
}

/**
 * Clone URL for PROJECT/repo. Prefers the shape of an existing clone's
 * origin (so SSH users get SSH, with the same host/port/user) and falls
 * back to the git host's page origin over HTTPS. Returns null if
 * neither is available.
 */
function deriveCloneUrl({ existingOrigins = [], pageOrigin, project, repo }) {
  const rules = gitRemoteRules();
  const tail = rules.cloneUrlTail(project, repo);
  for (const origin of existingOrigins) {
    if (!rules.parseOrigin(origin)) continue;
    return origin.trim().replace(/[^/:]+\/[^/]+?(?:\.git)?\/?$/, tail);
  }
  return pageOrigin ? rules.pageCloneUrl(pageOrigin, project, repo) : null;
}

/** Where a new clone should go: the folder most of the mapped clones share,
 * else the first common clone folder that exists, else ~/gitviews. */
function cloneRoot(existingRepos, searchRoots = defaultSearchRoots()) {
  const counts = new Map();
  for (const p of Object.values(existingRepos)) {
    const parent = path.dirname(p);
    counts.set(parent, (counts.get(parent) || 0) + 1);
  }
  const [mostCommon] = [...counts.entries()].sort((a, b) => b[1] - a[1])[0] || [];
  return mostCommon || searchRoots[0] || path.join(os.homedir(), "gitviews");
}

/**
 * Given the existing repos map (project/repo -> local path), try to find
 * where an unmapped project/repo might already be cloned locally, by
 * checking the sibling of every already-known clone's directory — i.e.
 * "if sample-app lives at ~/gitviews/sample-app, maybe sample-service lives at
 * ~/gitviews/sample-service" — plus any `extraRoots` (see
 * defaultSearchRoots). Only ever returns a candidate whose *origin* remote
 * is confirmed (via originMatchesProjectRepo) to actually point at this
 * exact project/repo — directory existence alone is never enough to
 * auto-adopt a mapping.
 */
function inferRepoPath(existingRepos, project, repo, extraRoots = []) {
  const roots = new Set([...Object.values(existingRepos).map((p) => path.dirname(p)), ...extraRoots]);
  for (const root of roots) {
    const candidate = path.join(root, repo);
    if (!checkRepoPath(candidate).ok) continue;
    if (originMatchesProjectRepo(getOriginUrl(candidate), project, repo)) {
      return candidate;
    }
  }
  return null;
}

// Looked up by name from core/feature-registry.js's per-feature
// `requiredChecks` — lets setup.js and server.ts run only the checks the
// *enabled* features actually need (e.g. skip requiring the `claude` CLI
// entirely if no enabled feature uses it), without either of them needing
// to hardcode which check functions exist.
const CHECKS_BY_NAME = {
  git: checkGit,
  claudeCli: checkClaudeCli,
  claudeAuth: checkClaudeAuth,
};

/**
 * Which of review-in-editor's supported editors are actually usable on
 * this machine, in preference order — used both to default
 * config.reviewEditor at setup time and (via core/editor.ts, which
 * requires this same function rather than re-implementing detection) to
 * pick a fallback at runtime if reviewEditor was never set. Not a hard
 * prerequisite (see core/feature-registry.js's requiredChecks for
 * review-in-editor — a missing editor just makes this list shorter, never
 * a setup-blocking FAIL), so this returns a list rather than {ok,message}
 * like the CHECKS_BY_NAME functions above.
 */
function detectInstalledEditors() {
  const found = [];
  if (process.platform !== "darwin") return found;
  // Claude Code checked first: it's the default editor offered by the
  // setup wizard (and the runtime fallback below) whenever it's on PATH.
  try {
    execFileSync("claude", ["--version"], { stdio: "ignore" });
    found.push({ id: "claude-code", label: "Claude Code (opens a new terminal)" });
  } catch {
    // Not on PATH — not offered.
  }
  if (fs.existsSync("/Applications/Cursor.app")) {
    found.push({ id: "cursor", label: "Cursor" });
  }
  // VS Code is deliberately not detected: it's hidden for now (see
  // REVIEW_EDITORS in core/feature-registry.js).
  return found;
}

/** The editor to use: `configured` when it's one of the supported ids
 * (claude-code, cursor), else the first installed one. A saved "vscode"
 * from before it was hidden counts as unset. */
function resolveReviewEditor(configured) {
  if (configured === "claude-code" || configured === "cursor") return configured;
  return detectInstalledEditors()[0]?.id;
}

module.exports = {
  MIN_NODE,
  MIN_NODE_MAJOR,
  CLAUDE_AUTH_TIMEOUT_MS,
  CHECKS_BY_NAME,
  checkNodeVersion,
  nodeMeetsMinimum,
  checkGit,
  checkClaudeCli,
  checkClaudeAuth,
  checkPortFree,
  checkRepoPath,
  getOriginUrl,
  originMatchesProjectRepo,
  parseBitbucketOrigin,
  parseGithubOrigin,
  gitRemoteRules,
  GIT_REMOTE_RULES,
  defaultSearchRoots,
  discoverLocalClones,
  isSafeRepoSegment,
  deriveCloneUrl,
  cloneRoot,
  inferRepoPath,
  detectInstalledEditors,
  resolveReviewEditor,
};
