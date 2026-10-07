// Local git for the ticket features: find a ticket's branches and
// worktrees in a clone, create its persistent worktree
// (~/.ai-dev-companion/<repo>.worktrees/<KEY>, the same layout as core/review-worktree.js's
// pr-review), and read or push its commits. Plain JS so node:test runs it
// against real throwaway repos. Every git call is an argv array
// (execFile-style, no shell); branch names are checked before use.
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");
const { repoWorktreesRoot } = require("./paths.js");
const { linkNodeModules } = require("./review-worktree.js");
const { isSafeBranchName } = require("./bitbucket-endpoints.js");
const { redactSecrets } = require("./history-record.js");

const ISSUE_KEY_RE = /^[A-Z][A-Z0-9_]*-[1-9]\d*$/;

const GIT_TIMEOUT_MS = 60 * 1000;

/** Runs git with an argv array. No stdin, a timeout that kills the child, and
 * error text with URL credentials and tokens masked (it ends up in job.error). */
function defaultGit(args, cwd, { allowFailure = false, timeoutMs = GIT_TIMEOUT_MS } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn("git", args, {
      cwd,
      shell: false,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, GIT_TERMINAL_PROMPT: "0" },
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
      // Don't wait for 'close': a grandchild (a hook) can keep the pipes open.
      reject(new Error(`git ${args[0]} timed out after ${Math.round(timeoutMs / 1000)} s and was stopped.`));
    }, timeoutMs);
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", (err) => {
      clearTimeout(timer);
      reject(new Error(redactSecrets(err.message)));
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (timedOut) return;
      if (code !== 0 && !allowFailure) {
        reject(new Error(redactSecrets(`git ${args[0]} failed (exit ${code}): ${(stderr || stdout).trim().slice(0, 2000)}`)));
        return;
      }
      resolve({ stdout, stderr, code });
    });
  });
}

function assertIssueKey(key) {
  if (typeof key !== "string" || !ISSUE_KEY_RE.test(key)) throw new Error(`Not an issue key: ${String(key).slice(0, 40)}`);
}

/** Matches the key inside a branch or folder name, case-insensitively, and
 * not as a prefix of a longer number (PROJ-12 doesn't match PROJ-123). */
function keyPattern(issueKey) {
  assertIssueKey(issueKey);
  return new RegExp(`(^|[^A-Za-z0-9])${issueKey}(?![0-9])`, "i");
}

/** `~/.ai-dev-companion/<basename>.worktrees/<KEY>`. */
function ticketWorktreePath(repoPath, issueKey) {
  assertIssueKey(issueKey);
  return path.join(repoWorktreesRoot(repoPath), issueKey);
}

const FOR_EACH_REF_FORMAT = "%(refname)%09%(objectname)%09%(upstream:short)%09%(upstream:track)";

/** "[ahead 2, behind 1]" / "[gone]" / "" -> counts. */
function parseTrack(track) {
  const t = String(track || "");
  const ahead = /ahead (\d+)/.exec(t);
  const behind = /behind (\d+)/.exec(t);
  return { ahead: ahead ? Number(ahead[1]) : 0, behind: behind ? Number(behind[1]) : 0, gone: t.includes("gone") };
}

/**
 * `git for-each-ref --format=FOR_EACH_REF_FORMAT refs/heads refs/remotes/origin`
 * output -> the branches whose name has the key, local and origin merged:
 * `[{ name, local, remote, sha, upstream, ahead, behind, gone }]`.
 */
function parseForEachRef(stdout, issueKey) {
  const re = keyPattern(issueKey);
  const byName = new Map();
  for (const line of String(stdout || "").split("\n")) {
    if (!line.trim()) continue;
    const [ref, sha, upstream, track] = line.split("\t");
    let name;
    let remote = false;
    if (ref.startsWith("refs/heads/")) name = ref.slice("refs/heads/".length);
    else if (ref.startsWith("refs/remotes/origin/")) {
      name = ref.slice("refs/remotes/origin/".length);
      remote = true;
    } else continue;
    if (name === "HEAD" || !re.test(name)) continue;
    const entry = byName.get(name) || { name, local: false, remote: false, sha: null, upstream: null, ahead: 0, behind: 0, gone: false };
    if (remote) {
      entry.remote = true;
      if (!entry.sha) entry.sha = sha || null;
    } else {
      entry.local = true;
      entry.sha = sha || null;
      entry.upstream = upstream || null;
      Object.assign(entry, parseTrack(track));
    }
    byName.set(name, entry);
  }
  return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}

/** `git worktree list --porcelain` -> `[{ dir, head, branch, detached }]`. */
function parseWorktreePorcelain(stdout) {
  const out = [];
  let cur = null;
  for (const line of String(stdout || "").split("\n")) {
    if (line.startsWith("worktree ")) {
      cur = { dir: line.slice("worktree ".length), head: null, branch: null, detached: false };
      out.push(cur);
    } else if (!cur) continue;
    else if (line.startsWith("HEAD ")) cur.head = line.slice(5);
    else if (line.startsWith("branch ")) cur.branch = line.slice(7).replace(/^refs\/heads\//, "");
    else if (line === "detached") cur.detached = true;
  }
  return out;
}

/** Changed paths in `git status --porcelain` output. */
function countChanged(stdout) {
  return String(stdout || "").split("\n").filter((l) => l.trim()).length;
}

const MAX_WORKTREES = 5;

/**
 * What one clone has for this ticket: `{ branches, worktrees }` —
 * worktrees whose branch or folder name has the key, each with `dirty`,
 * `changed` and, when its branch is known, `ahead`/`behind` from the
 * branch list. The clone's own main checkout is never listed.
 */
async function scanRepoForTicket(repoPath, issueKey, { git = defaultGit } = {}) {
  const re = keyPattern(issueKey);
  const refs = await git(["for-each-ref", `--format=${FOR_EACH_REF_FORMAT}`, "refs/heads", "refs/remotes/origin"], repoPath);
  const branches = parseForEachRef(refs.stdout, issueKey);
  const list = await git(["worktree", "list", "--porcelain"], repoPath);
  const main = path.resolve(repoPath);
  const worktrees = [];
  for (const wt of parseWorktreePorcelain(list.stdout)) {
    if (path.resolve(wt.dir) === main) continue;
    if (!(wt.branch && re.test(wt.branch)) && !re.test(path.basename(wt.dir))) continue;
    if (worktrees.length >= MAX_WORKTREES) break;
    const exists = fs.existsSync(wt.dir);
    let changed = 0;
    if (exists) {
      const st = await git(["status", "--porcelain"], wt.dir, { allowFailure: true });
      changed = st.code === 0 ? countChanged(st.stdout) : 0;
    }
    const b = branches.find((x) => x.name === wt.branch);
    worktrees.push({
      dir: wt.dir,
      branch: wt.branch,
      head: wt.head,
      exists,
      dirty: changed > 0,
      changed,
      ahead: b ? b.ahead : 0,
      behind: b ? b.behind : 0,
      isTicketWorktree: path.resolve(wt.dir) === path.resolve(ticketWorktreePath(repoPath, issueKey)),
    });
  }
  return { branches, worktrees };
}

/** origin/HEAD's branch if the clone knows it, else master, else main. */
async function defaultBranchFromGit(repoPath, { git = defaultGit } = {}) {
  const sym = await git(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"], repoPath, { allowFailure: true });
  if (sym.code === 0 && sym.stdout.trim()) return sym.stdout.trim().replace(/^refs\/remotes\/origin\//, "");
  for (const candidate of ["master", "main"]) {
    const r = await git(["rev-parse", "--verify", "--quiet", `refs/remotes/origin/${candidate}`], repoPath, { allowFailure: true });
    if (r.code === 0) return candidate;
  }
  throw new Error(`Couldn't tell the default branch of ${repoPath} (no origin/HEAD, origin/master or origin/main).`);
}

async function refExists(repoPath, ref, git) {
  const r = await git(["rev-parse", "--verify", "--quiet", ref], repoPath, { allowFailure: true });
  return r.code === 0;
}

/**
 * Makes sure `~/.ai-dev-companion/<repo>.worktrees/<KEY>` exists as a worktree of `repoPath`
 * and returns `{ dir, branch, created }`. Idempotent: an existing worktree
 * there is reused on whatever branch it is on. Otherwise it fetches
 * `origin/<base>` and checks out `branch` — the existing local branch, the
 * origin branch of that name, or a new branch from `origin/<base>`
 * (--no-track, so the upstream becomes origin/<branch> on the first push).
 * A folder at that path that isn't a worktree of this repo is never
 * touched: that is an error.
 */
async function ensureTicketWorktree(repoPath, { issueKey, branch, base }, { git = defaultGit } = {}) {
  const dir = ticketWorktreePath(repoPath, issueKey);
  const list = await git(["worktree", "list", "--porcelain"], repoPath);
  const target = fs.existsSync(dir) ? fs.realpathSync(dir) : path.resolve(dir);
  const existing = parseWorktreePorcelain(list.stdout).find(
    (wt) => (fs.existsSync(wt.dir) ? fs.realpathSync(wt.dir) : path.resolve(wt.dir)) === target,
  );
  if (existing) return { dir, branch: existing.branch || branch, created: false };
  if (fs.existsSync(dir)) {
    throw new Error(`${dir} already exists but isn't a worktree of ${repoPath}. Move it aside and try again.`);
  }
  if (!isSafeBranchName(branch)) throw new Error(`Not a safe branch name: ${String(branch).slice(0, 80)}`);
  if (!isSafeBranchName(base)) throw new Error(`Not a safe base branch: ${String(base).slice(0, 80)}`);
  await git(["check-ref-format", "--branch", branch], repoPath);
  await git(["fetch", "origin", `refs/heads/${base}:refs/remotes/origin/${base}`], repoPath);
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  if (await refExists(repoPath, `refs/heads/${branch}`, git)) {
    await git(["worktree", "add", dir, branch], repoPath);
  } else if (await refExists(repoPath, `refs/remotes/origin/${branch}`, git)) {
    await git(["worktree", "add", "--track", "-b", branch, dir, `origin/${branch}`], repoPath);
  } else {
    await git(["worktree", "add", "--no-track", "-b", branch, dir, `origin/${base}`], repoPath);
  }
  linkNodeModules(repoPath, dir);
  return { dir, branch, created: true };
}

/** Refresh refs/remotes/origin/<base> so the ahead count, log and diff below
 * aren't measured against a stale ref. Throws if origin can't be reached. */
async function fetchBase(dir, base, { git = defaultGit } = {}) {
  if (!isSafeBranchName(base)) throw new Error("Not a safe base branch.");
  await git(["fetch", "origin", `+refs/heads/${base}:refs/remotes/origin/${base}`], dir);
}

/** The branch checked out in dir ("HEAD" when detached). */
async function currentBranch(dir, { git = defaultGit } = {}) {
  const r = await git(["rev-parse", "--abbrev-ref", "HEAD"], dir);
  return r.stdout.trim();
}

/** Commits on HEAD that origin/<base> doesn't have. */
async function commitsAhead(dir, base, { git = defaultGit } = {}) {
  if (!isSafeBranchName(base)) throw new Error("Not a safe base branch.");
  const r = await git(["rev-list", "--count", `refs/remotes/origin/${base}..HEAD`], dir);
  return Number(r.stdout.trim()) || 0;
}

/** Subjects and bodies of those commits (newest first), capped. */
async function commitLog(dir, base, { git = defaultGit, max = 30, maxChars = 8000 } = {}) {
  if (!isSafeBranchName(base)) throw new Error("Not a safe base branch.");
  const r = await git(["log", "--no-merges", `--max-count=${max}`, "--format=- %s%n%b", `refs/remotes/origin/${base}..HEAD`], dir);
  return r.stdout.replace(/\n{3,}/g, "\n\n").trim().slice(0, maxChars);
}

/** `git diff --stat` of the branch against the merge base with origin/<base>, capped. */
async function diffStat(dir, base, { git = defaultGit, maxChars = 4000 } = {}) {
  if (!isSafeBranchName(base)) throw new Error("Not a safe base branch.");
  const r = await git(["diff", "--stat", `refs/remotes/origin/${base}...HEAD`], dir);
  return r.stdout.trim().slice(0, maxChars);
}

/** `git push -u origin <branch>` from the worktree (hooks run as usual). */
async function pushBranch(dir, branch, { git = defaultGit } = {}) {
  if (!isSafeBranchName(branch)) throw new Error("Not a safe branch name.");
  await git(["push", "-u", "origin", branch], dir);
}

module.exports = {
  defaultGit,
  FOR_EACH_REF_FORMAT,
  keyPattern,
  ticketWorktreePath,
  parseTrack,
  parseForEachRef,
  parseWorktreePorcelain,
  countChanged,
  scanRepoForTicket,
  defaultBranchFromGit,
  ensureTicketWorktree,
  fetchBase,
  currentBranch,
  commitsAhead,
  commitLog,
  diffStat,
  pushBranch,
};
