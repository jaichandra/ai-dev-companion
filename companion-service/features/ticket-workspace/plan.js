// Pure logic for the ticket workspace: which history keys to follow, and how
// the live git scan, Bitbucket reads and history combine into one view of a
// ticket (worktrees, branches, PRs with build state, analyses, sessions) plus
// what to offer next. index.ts does the I/O. Plain JS so node:test runs it.
const resume = require("../../core/resume.js");
const { isSafeBranchName } = require("../../core/bitbucket-endpoints.js");
const { gitRemoteRules } = require("../../core/prereqs.js");

const ISSUE_KEY_RE = /^[A-Za-z][A-Za-z0-9_]*-[1-9]\d*$/;
const PR_KEY_RE = /^(?:bitbucket|github):((?:[A-Za-z0-9]|\.[A-Za-z0-9])[A-Za-z0-9._-]*)\/((?:[A-Za-z0-9]|\.[A-Za-z0-9])[A-Za-z0-9._-]*)#([1-9]\d*)$/;
const MAX_RELATED = 20;
const MAX_SESSIONS = 10;

/** `{issueKey}` -> the upper-cased key, or throws. */
function parseWorkspacePayload(payload) {
  const raw = payload && typeof payload === "object" ? payload.issueKey : undefined;
  if (typeof raw !== "string" || !ISSUE_KEY_RE.test(raw.trim())) {
    throw new Error('ticket-workspace payload must include an issueKey such as "PROJ-1234".');
  }
  return { issueKey: raw.trim().toUpperCase() };
}

/** The neighbour keys of a history detail worth a second look, by kind. */
function relatedKeys(detail) {
  const out = { worktree: [], pr: [], analysis: [], job: [], session: [] };
  const edges = detail && Array.isArray(detail.edges) ? detail.edges : [];
  for (const e of edges) {
    if (!e || typeof e.key !== "string" || !(e.kind in out)) continue;
    if (!out[e.kind].includes(e.key)) out[e.kind].push(e.key);
  }
  for (const kind of Object.keys(out)) out[kind] = out[kind].slice(0, MAX_RELATED);
  return out;
}

function parsePrKey(key) {
  const m = typeof key === "string" ? PR_KEY_RE.exec(key) : null;
  return m ? { project: m[1], repo: m[2], id: Number(m[3]) } : null;
}

/** A branch name from Bitbucket is untrusted: one that isn't safe becomes unknown (null). */
const safeBranch = (name) => (typeof name === "string" && isSafeBranchName(name) ? name : null);

/** Only a PR page of the git host (Bitbucket or GitHub) on its configured address is ever offered as a link. */
function prUrlAllowed(url, gitBaseUrl) {
  try {
    const u = new URL(url);
    const base = new URL(gitBaseUrl);
    return u.protocol === "https:" && u.origin === base.origin && gitRemoteRules().parsePrUrl(u.pathname) !== null;
  } catch {
    return false;
  }
}

/**
 * What the history knows, from the ticket's detail and the details of its
 * neighbours (`related`: key -> getItem result, for up to two hops):
 * `{ worktrees, prs, analyses, sessions, notes }`.
 *
 * A history row is untrusted (the database is a local file), so a folder in
 * it is only used if `policy` allows it: `worktreeOk({ repoKey, dir })` (the
 * folder must be that repo's own ticket worktree) and `sessionCwdOk(cwd)`
 * (the job-file allow-list). With no policy nothing is allowed; each
 * dropped row becomes a line in `notes`.
 */
function historyFacts(ticket, related, policy = {}) {
  const facts = { worktrees: [], prs: [], analyses: [], sessions: [], notes: [] };
  const worktreeOk = typeof policy.worktreeOk === "function" ? policy.worktreeOk : () => false;
  const sessionCwdOk = typeof policy.sessionCwdOk === "function" ? policy.sessionCwdOk : () => false;
  if (!ticket) return facts;
  const rel = related || {};
  const keys = relatedKeys(ticket);
  for (const key of keys.worktree) {
    const d = rel[key] && rel[key].item && rel[key].item.data;
    if (d && typeof d.dir === "string") {
      if (!worktreeOk({ repoKey: d.repoKey, dir: d.dir })) {
        facts.notes.push(`Local history: ignored a worktree (${d.dir}) that isn't this ticket's worktree in a configured repository.`);
        continue;
      }
      facts.worktrees.push({ dir: d.dir, repoKey: d.repoKey || null, branch: d.branch || null, base: d.base || null, fixStartedAt: d.fixStartedAt ?? null });
    }
  }
  for (const key of keys.pr) {
    const pr = parsePrKey(key);
    const item = rel[key] && rel[key].item;
    if (pr) facts.prs.push({ ...pr, title: item ? item.title : null, url: item ? item.url : null });
  }
  for (const key of keys.analysis) {
    const item = rel[key] && rel[key].item;
    if (item) facts.analyses.push({ key, title: item.title, excerpt: item.excerpt, updatedAt: item.updatedAt });
  }
  const sessionKeys = new Set();
  for (const d of Object.values(rel)) {
    for (const k of relatedKeys(d).session) sessionKeys.add(k);
  }
  for (const key of sessionKeys) {
    const item = rel[key] && rel[key].item;
    if (!item || !item.data) continue;
    const session = { id: key.replace(/^session:/, ""), cwd: item.data.cwd, permissionMode: item.data.permissionMode };
    if (!resume.validateSession(session)) continue;
    if (!sessionCwdOk(session.cwd)) {
      facts.notes.push(`Local history: ignored a Claude session whose folder (${session.cwd}) isn't one this service uses.`);
      continue;
    }
    facts.sessions.push({ ...session, at: item.updatedAt || 0 });
  }
  return facts;
}

/** Keys to fetch for the second and third hop: the ticket's neighbours, then their sessions. */
function secondHopKeys(ticket) {
  const k = relatedKeys(ticket);
  return [...k.worktree, ...k.pr, ...k.analysis, ...k.job];
}
function sessionKeysOf(details) {
  const out = [];
  for (const d of details) for (const key of relatedKeys(d).session) if (!out.includes(key)) out.push(key);
  return out.slice(0, MAX_SESSIONS);
}

/**
 * The workspace model. `repos`: `[{ repoKey, branches, worktrees, prs }]`
 * from the live scan (each PR may carry `build`); `history`: historyFacts'
 * result; `exists(dir)`: whether a folder is still there; `cwdAllowed(dir)`:
 * whether a session may be started in it (the resume fallback folder — with
 * no predicate there is none).
 */
function buildWorkspace({ issueKey, bitbucketBaseUrl, repos, history, notes = [], exists = () => true, cwdAllowed = () => false }) {
  const worktrees = [];
  const branches = [];
  const prs = [];
  for (const r of repos || []) {
    for (const w of r.worktrees || []) worktrees.push({ repoKey: r.repoKey, ...w });
    for (const b of r.branches || []) branches.push({ repoKey: r.repoKey, ...b });
    for (const pr of r.prs || []) {
      if (!prUrlAllowed(pr.url, bitbucketBaseUrl)) continue;
      if (prs.some((p) => p.repoKey === r.repoKey && p.id === pr.id)) continue;
      prs.push({
        repoKey: r.repoKey,
        id: pr.id,
        title: pr.title ?? null,
        state: pr.state ?? null,
        url: pr.url,
        fromBranch: safeBranch(pr.fromBranch),
        toBranch: safeBranch(pr.toBranch),
        build: pr.build
          ? {
              state: pr.build.state,
              counts: pr.build.counts,
              builds: (pr.build.builds || []).map((b) => ({
                state: b.state,
                name: typeof b.name === "string" ? b.name : null,
                url: typeof b.url === "string" ? b.url : null,
              })),
            }
          : null,
        source: "bitbucket",
      });
    }
  }
  const h = history || { worktrees: [], prs: [], analyses: [], sessions: [] };
  for (const n of h.notes || []) if (!notes.includes(n)) notes.push(n);
  for (const w of h.worktrees) {
    if (worktrees.some((x) => x.dir === w.dir)) continue;
    worktrees.push({
      repoKey: w.repoKey,
      dir: w.dir,
      branch: w.branch,
      exists: exists(w.dir),
      dirty: false,
      changed: 0,
      ahead: 0,
      behind: 0,
      isTicketWorktree: true,
      fromHistory: true,
    });
  }
  const base = (() => {
    try {
      return new URL(bitbucketBaseUrl).origin;
    } catch {
      return null;
    }
  })();
  for (const p of h.prs) {
    const repoKey = `${p.project}/${p.repo}`;
    if (prs.some((x) => x.id === p.id && x.repoKey.toLowerCase() === repoKey.toLowerCase())) continue;
    const url = p.url && prUrlAllowed(p.url, bitbucketBaseUrl) ? p.url : base ? `${base}${gitRemoteRules().prPath(p.project, p.repo, p.id)}` : null;
    if (!url) continue;
    prs.push({ repoKey, id: p.id, title: p.title, state: null, url, fromBranch: null, toBranch: null, build: null, source: "history" });
  }
  const sessions = [];
  for (const s of [...h.sessions].sort((a, b) => (b.at || 0) - (a.at || 0))) {
    if (!sessions.some((x) => x.id === s.id)) sessions.push(s);
  }
  sessions.splice(MAX_SESSIONS);

  const live = worktrees.filter((w) => w.exists !== false);
  const ticketWorktree = live.find((w) => w.isTicketWorktree) || null;
  const openFor = (w) => prs.some((p) => p.state === "OPEN" && p.fromBranch === w.branch && p.repoKey === w.repoKey);
  const createPr =
    ticketWorktree && ticketWorktree.branch && ticketWorktree.repoKey && !openFor(ticketWorktree)
      ? { repoKey: ticketWorktree.repoKey, dir: ticketWorktree.dir, branch: ticketWorktree.branch }
      : null;
  const startFix = live.length === 0 && branches.length === 0;
  const preferred = ticketWorktree ? sessions.find((s) => s.cwd === ticketWorktree.dir) : undefined;
  return {
    issueKey,
    worktrees,
    branches,
    prs,
    analyses: h.analyses,
    sessions,
    notes,
    suggestions: { startFix, createPr },
    resume: {
      session: preferred || sessions[0] || null,
      worktreeDir: [ticketWorktree, ...live].find((w) => w && cwdAllowed(w.dir))?.dir || null,
    },
  };
}

/** One line for the job's result and the MCP/CLI output. */
function workspaceSummary(ws) {
  const n = (count, word) => `${count} ${word}${count === 1 ? "" : "s"}`;
  const open = ws.prs.filter((p) => p.state === "OPEN").length;
  const parts = [
    n(ws.worktrees.filter((w) => w.exists !== false).length, "worktree"),
    n(ws.branches.length, "branch").replace("branchs", "branches"),
    `${n(ws.prs.length, "PR")}${open ? ` (${open} open)` : ""}`,
    n(ws.analyses.length, "analysis").replace("analysiss", "analyses"),
    n(ws.sessions.length, "Claude session"),
  ];
  return `${ws.issueKey}: ${parts.join(", ")}.`;
}

module.exports = {
  parseWorkspacePayload,
  relatedKeys,
  secondHopKeys,
  sessionKeysOf,
  parsePrKey,
  prUrlAllowed,
  historyFacts,
  buildWorkspace,
  workspaceSummary,
};
