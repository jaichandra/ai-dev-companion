// Everything the PR-page features need to know about WHICH git host they are on, so the features themselves
// (resolve conflict, address review comments, review in editor) read the same on Bitbucket and on GitHub:
// what a pull request page's address looks like, the key a PR is filed under, its page address, and how to
// read its state. The provider comes from GET /targets (`providers.git`); until that answers it is Bitbucket's.
//
//   Bitbucket Server  /projects/<PROJECT>/repos/<repo>/pull-requests/<n>
//                     read with same-origin REST calls: the page's own login does the work
//   GitHub            /<owner>/<repo>/pull/<n>
//                     read through the companion (GET /prs/status): GitHub's page login doesn't authenticate its
//                     API, so the companion asks with its saved token
//
// `pr()` answers { state, conflicted, isFork, fromBranch, toBranch, title, commentCount, openTaskCount } with null
// for whatever the host can't say, or null altogether when the PR couldn't be read.
(function (root) {
  const DEFAULT_PROVIDER = "bitbucket-dc";
  let current = DEFAULT_PROVIDER;

  /** A PR page's address tested against location.href: capture groups are project (owner), repo, number. */
  const URL_PATTERNS = {
    "bitbucket-dc": /\/projects\/([^/]+)\/repos\/([^/]+)\/pull-requests\/(\d+)/,
    github: /^https?:\/\/[^/]+\/([^/]+)\/([^/]+)\/pull\/(\d+)(?=[/?#]|$)/,
  };

  const KEYS = {
    "bitbucket-dc": (project, repo, id) => `bitbucket:${project.toUpperCase()}/${repo.toLowerCase()}#${id}`,
    github: (project, repo, id) => `github:${project.toLowerCase()}/${repo.toLowerCase()}#${id}`,
  };

  const PAGES = {
    "bitbucket-dc": (origin, project, repo, id) => `${origin}/projects/${project}/repos/${repo}/pull-requests/${id}`,
    github: (origin, project, repo, id) => `${origin}/${project}/${repo}/pull/${id}`,
  };

  function providerOf(targets) {
    const id = targets && targets.providers && targets.providers.git;
    return URL_PATTERNS[id] ? id : current;
  }

  /** Called when /targets answers, so functions that get no targets (scopeKey) use the right host. */
  function configure(targets) {
    current = providerOf(targets);
  }

  /** The PR page pattern for the targets (a function of the targets, as features' urlPattern may be). */
  function urlPattern(targets) {
    return URL_PATTERNS[providerOf(targets)];
  }

  /** The key a job for this PR is filed under — the same string core/prereqs.js's prKey makes. */
  function scopeKey(match) {
    return KEYS[current](match[1], match[2], match[3]);
  }

  /** The PR's own page address. */
  function prUrl(ctx, ids) {
    return PAGES[providerOf(ctx.targets)](ctx.origin, ids.project, ids.repo, ids.prId);
  }

  /** What Bitbucket's REST says, in the shared shape. `want.conflicts` reads the merge check first and, when the
   * PR doesn't conflict, stops there (the caller has no use for the rest); `want.properties` asks for comment counts. */
  async function bitbucketPr(ctx, ids, want) {
    const base = `${ctx.origin}/rest/api/1.0/projects/${ids.project}/repos/${ids.repo}/pull-requests/${ids.prId}`;
    let conflicted = null;
    if (want.conflicts) {
      // credentials: "include" isn't strictly needed (a same-origin request sends the session cookie anyway) but
      // it is the property these calls depend on, so it stays explicit.
      const mergeRes = await fetch(`${base}/merge`, { credentials: "include" });
      if (!mergeRes.ok) return null;
      conflicted = !!(await mergeRes.json()).conflicted;
      if (!conflicted) return { state: null, conflicted: false, isFork: null, fromBranch: null, toBranch: null, title: null, commentCount: null, openTaskCount: null };
    }
    const res = await fetch(`${base}${want.properties ? "?withProperties=true" : ""}`, { credentials: "include" });
    if (!res.ok) return null;
    const pr = await res.json();
    const props = pr && typeof pr.properties === "object" && pr.properties ? pr.properties : {};
    return {
      state: typeof pr.state === "string" ? pr.state : null,
      conflicted,
      isFork: null,
      fromBranch: (pr.fromRef && pr.fromRef.displayId) || null,
      toBranch: (pr.toRef && pr.toRef.displayId) || null,
      title: typeof pr.title === "string" ? pr.title : null,
      commentCount: typeof props.commentCount === "number" ? props.commentCount : null,
      openTaskCount: typeof props.openTaskCount === "number" ? props.openTaskCount : null,
    };
  }

  /** What the companion says about the PR (GET /prs/status), which is already in the shared shape. */
  async function githubPr(ctx, ids) {
    const answer = await ctx.service("pr-status", { project: ids.project, repo: ids.repo, prId: ids.prId });
    return answer && answer.status ? answer.status : null;
  }

  /**
   * Reads the PR of this page: `ids` is { project, repo, prId }. Never throws: a host that can't be reached, or
   * a PR that isn't found, is null (the feature simply isn't listed).
   */
  async function pr(ctx, ids, want = {}) {
    try {
      return providerOf(ctx.targets) === "github" ? await githubPr(ctx, ids) : await bitbucketPr(ctx, ids, want);
    } catch {
      return null;
    }
  }

  /** One summary line for a queued ("pending-start") job's own start payload — {project, repo, prId, sourceBranch?,
   * destBranch?}. Anything else renders no summary line. Host-neutral: it only reads the payload. */
  function prSummary(payload) {
    if (!payload || typeof payload !== "object" || payload.prId === undefined) return null;
    const { prId, sourceBranch, destBranch } = payload;
    return sourceBranch && destBranch ? `PR #${prId} · ${sourceBranch} ← ${destBranch}` : `PR #${prId}`;
  }

  const api = { configure, urlPattern, scopeKey, prUrl, pr, prSummary, URL_PATTERNS, KEYS, PAGES };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.PaiGit = api;
})(typeof self !== "undefined" ? self : this);
