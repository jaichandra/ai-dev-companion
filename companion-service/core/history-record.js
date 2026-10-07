// Turns a job's terminal transition into history rows. Pure mapping
// (opsForJob) plus a thin apply step; core/jobs.ts's onTransition hook calls
// the recorder. Personal data only; text is clipped by the store.

// Which features get which treatment is declared by each feature (`history` in its
// features/<id>/feature.js); these lists are read from the loaded packs.
const packs = require("./packs.js");
const { gitRemoteRules } = require("./prereqs.js");
/** Features whose job "finishes" at awaiting-approval (they have nothing to approve). */
const READ_ONLY_FEATURES = packs.idsWithHistoryFlag("readOnly");
/** Features whose awaiting-approval is a milestone worth recording on its
 * own ("completed"), before the approve that ends them: Ticket to PR's
 * Start fix has made the worktree by then, which the ticket workspace
 * should find even if no PR is ever opened. */
const MILESTONE_FEATURES = packs.idsWithHistoryFlag("milestone");
/** Features that record only their timing event, no items: a workspace scan
 * reads the history, and one job item per click would only clutter it. */
const EVENT_ONLY_FEATURES = packs.idsWithHistoryFlag("eventOnly");

function outcomeFor(job) {
  if (job.status === "approved") return "approved";
  // Discarding a finished read-only analysis is not a job outcome; its "completed" event stands.
  if (job.status === "rejected") return READ_ONLY_FEATURES.includes(job.featureId) ? null : "discarded";
  if (job.status === "failed") return "failed";
  if (
    job.status === "awaiting-approval" &&
    (READ_ONLY_FEATURES.includes(job.featureId) || MILESTONE_FEATURES.includes(job.featureId))
  ) {
    return "completed";
  }
  return null;
}

/** Masks credentials that could ride along in an error message (argv, stderr). */
function redactSecrets(text) {
  if (typeof text !== "string") return text;
  return text
    .replace(/\b(Bearer|Basic)\s+[^\s"'&]+/gi, "$1 ***")
    .replace(/\b(token|password|secret|apikey|api_key)=[^\s&"']*/gi, "$1=***")
    .replace(/\b([a-z][a-z0-9+.-]*):\/\/[^\s/@]+@/gi, "$1://***@");
}

function metricsFor(job, at) {
  const metrics = { durationMs: Math.max(0, at - job.createdAt) };
  const data = job.data || {};
  if (job.startedVia === "watcher" && typeof job.watcher === "string") {
    metrics.watcher = job.watcher;
    metrics.tier = "claude";
  }
  if (job.featureId === "resolve-conflict") {
    metrics.conflicted = data.conflicted === true;
    if (job.result && Array.isArray(job.result.files)) metrics.filesChanged = job.result.files.length;
  }
  if (job.featureId === "ticket-to-pr" && job.status === "approved" && Number.isFinite(data.fixStartedAt)) {
    // The headline number: from Start fix to the PR being open.
    metrics.startFixToPrMs = Math.max(0, at - data.fixStartedAt);
  }
  return metrics;
}

function analysisExcerpt(analysis) {
  if (typeof analysis === "string") return analysis;
  if (analysis && typeof analysis === "object") {
    // analyze-issue's report leads with "tldr"; the others are older shapes.
    for (const field of ["tldr", "summary", "rootCause", "title"]) {
      if (typeof analysis[field] === "string") return analysis[field];
    }
  }
  return undefined;
}

function subjectFor(job, data) {
  const scopeKey = job.scopeKey;
  if (typeof scopeKey !== "string") return null;
  if (scopeKey.startsWith("jira:")) {
    return { ref: "subject", kind: "ticket", key: scopeKey, title: typeof data.summary === "string" ? data.summary : undefined };
  }
  const m = /^(?:bitbucket|github):([^/]+)\/([^#]+)#(\d+)$/.exec(scopeKey);
  if (m) {
    return {
      ref: "subject",
      kind: "pr",
      key: scopeKey,
      repo: `${m[1]}/${m[2]}`,
      title: data.pr && typeof data.pr.title === "string" ? data.pr.title : undefined,
    };
  }
  return null;
}


/** Ticket to PR's worktree and PR, linked from the ticket. */
function ticketToPrOps(data, items, edges, at) {
  const wt = data.ticketWorktree;
  if (wt && typeof wt.dir === "string" && wt.dir.startsWith("/")) {
    items.push({
      ref: "worktree",
      kind: "worktree",
      key: `worktree:${wt.dir}`,
      repo: typeof data.repoKey === "string" ? data.repoKey : undefined,
      title: typeof data.branch === "string" ? data.branch : undefined,
      data: {
        dir: wt.dir,
        repoKey: data.repoKey,
        branch: data.branch,
        base: data.base,
        fixStartedAt: Number.isFinite(data.fixStartedAt) ? data.fixStartedAt : undefined,
      },
      at,
    });
    edges.push({ from: "subject", to: "worktree", rel: "worktree" });
  }
  const pr = data.pr;
  const parsed = pr && typeof pr.url === "string" ? gitRemoteRules().parsePrUrl(pr.url) : null;
  if (parsed && Number.isInteger(pr.id)) {
    items.push({
      ref: "pr",
      kind: "pr",
      key: gitRemoteRules().prKey(parsed.project, parsed.repo, pr.id),
      repo: `${parsed.project}/${parsed.repo}`,
      title: typeof pr.title === "string" ? pr.title : undefined,
      url: pr.url,
      at,
    });
    edges.push({ from: "subject", to: "pr", rel: "links" });
    if (items.some((i) => i.ref === "worktree")) edges.push({ from: "worktree", to: "pr", rel: "links" });
  }
}

function opsForJob(job, at) {
  const outcome = outcomeFor(job);
  if (!outcome) return null;
  const data = job.data || {};
  const items = [];
  const edges = [];
  const metrics = metricsFor(job, at);
  const event = {
    jobId: job.id,
    featureId: job.featureId,
    scopeKey: job.scopeKey,
    status: job.status,
    at,
    durationMs: metrics.durationMs,
    outcome,
    metrics,
  };
  if (EVENT_ONLY_FEATURES.includes(job.featureId)) return { items, edges, at, event };

  items.push({
    ref: "job",
    kind: "job",
    key: `job:${job.id}`,
    title: redactSecrets((job.result && job.result.summary) || `${job.featureId} ${job.status}`),
    excerpt: redactSecrets(job.error || (job.result && job.result.summary)),
    data: { featureId: job.featureId, status: job.status, scopeKey: job.scopeKey, startedVia: job.startedVia },
    at,
  });

  const subject = subjectFor(job, data);
  if (subject) {
    items.push({ ...subject, at });
    const rel = job.featureId === "analyze-issue" ? "links" : job.status === "approved" ? "fixes" : "links";
    edges.push({ from: "job", to: "subject", rel });

    if (job.featureId === "analyze-issue" && data.analysis) {
      items.push({
        ref: "analysis",
        kind: "analysis",
        key: `analysis:${job.scopeKey}`,
        repo: typeof data.repoKey === "string" ? data.repoKey : undefined,
        title: `Analysis of ${subject.key.replace(/^jira:/, "")}`,
        excerpt: analysisExcerpt(data.analysis),
        at,
      });
      edges.push({ from: "subject", to: "analysis", rel: "analyzed" });
    }
  }

  const session = data.claudeSession;
  if (session && typeof session.id === "string") {
    items.push({
      ref: "session",
      kind: "session",
      key: `session:${session.id}`,
      title: `${job.featureId} session`,
      data: { cwd: session.cwd, permissionMode: session.permissionMode },
      at,
    });
    edges.push({ from: "job", to: "session", rel: "links" });
  }

  if (job.featureId === "ticket-to-pr" && subject) {
    ticketToPrOps(data, items, edges, at);
    if (session && typeof session.id === "string" && items.some((i) => i.ref === "worktree")) {
      edges.push({ from: "worktree", to: "session", rel: "links" });
    }
  }

  return { items, edges, at, event };
}

function applyOps(history, ops) {
  const ids = new Map();
  for (const item of ops.items) {
    ids.set(
      item.ref,
      history.upsertItem({
        kind: item.kind,
        key: item.key,
        repo: item.repo,
        title: item.title,
        url: item.url,
        excerpt: item.excerpt,
        data: item.data,
        at: item.at,
      }),
    );
  }
  for (const edge of ops.edges) {
    const src = ids.get(edge.from);
    const dst = ids.get(edge.to);
    if (src !== undefined && dst !== undefined) history.addEdge({ src, dst, rel: edge.rel, at: ops.at });
  }
  history.recordEvent(ops.event);
}

/** A job that aged out before anyone approved or discarded it; null when its status already has an event. */
function expiredEvent(job, at) {
  if (outcomeFor(job) !== null) return null;
  const durationMs = Math.max(0, at - job.createdAt);
  return {
    jobId: job.id,
    featureId: job.featureId,
    scopeKey: job.scopeKey,
    status: "expired",
    at,
    durationMs,
    outcome: "expired",
    metrics: {
      durationMs,
      lastStatus: job.status,
      ...(job.startedVia === "watcher" && typeof job.watcher === "string" ? { watcher: job.watcher } : {}),
    },
  };
}

/** The inbox's "opened" click on a pre-warmed job: the "used" outcome of a
 * read-only result (an approve is how a writing one gets used). Null for a
 * job no watcher started. */
function usedEvent(job, at) {
  if (!job || job.startedVia !== "watcher" || typeof job.watcher !== "string") return null;
  return {
    jobId: job.id,
    featureId: job.featureId,
    scopeKey: job.scopeKey,
    status: "opened",
    at,
    durationMs: Math.max(0, at - job.createdAt),
    outcome: "used",
    metrics: { watcher: job.watcher, opened: true },
  };
}

function createRecorder(history, { log = () => {}, now = Date.now } = {}) {
  let frozen = false;
  return {
    onTransition(job) {
      if (frozen) return;
      try {
        const ops = opsForJob(job, now());
        if (ops) applyOps(history, ops);
      } catch (err) {
        log(`couldn't record job ${job && job.id} in the history: ${err.message}`);
      }
    },
    /** Stops recording — used at shutdown, when killed runs would otherwise log as failures. */
    freeze() {
      frozen = true;
    },
  };
}

module.exports = {
  READ_ONLY_FEATURES,
  MILESTONE_FEATURES,
  EVENT_ONLY_FEATURES,
  redactSecrets,
  outcomeFor,
  opsForJob,
  applyOps,
  expiredEvent,
  usedEvent,
  createRecorder,
};
