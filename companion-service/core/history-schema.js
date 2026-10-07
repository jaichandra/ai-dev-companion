// Pure pieces of the local history store (core/history-db.js opens the
// SQLite file): what may be stored, key normalization, excerpt limits, the
// SQL migrations and the metrics roll-up. Plain JS, no I/O, so node:test
// covers it straight from source. Personal data only — see the "Global
// Constraints" of the Phase 4 plan: never credentials, cookies or raw secrets.
const SCHEMA_VERSION = 2;
const KINDS = ["ticket", "pr", "analysis", "review_comment", "job", "session", "worktree"];
const RELS = ["analyzed", "fixes", "commented_on", "links", "worktree"];
const PROVENANCE = ["jenkins", "bitbucket", "github", "jira", "claude", "onprem-llm", "user"];
// "used": a pre-warmed read-only result the user opened from the inbox.
const OUTCOMES = ["approved", "discarded", "expired", "failed", "completed", "used"];
const EXCERPT_MAX = 2048;
const TITLE_MAX = 300;
const KEY_MAX = 300;
const DEFAULT_RETENTION_DAYS = 180;

const MIGRATIONS = [
  // v1
  `
  CREATE TABLE items (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    repo TEXT,
    title TEXT,
    url TEXT,
    excerpt TEXT,
    updated_at INTEGER NOT NULL,
    data_json TEXT,
    UNIQUE (kind, key)
  );
  CREATE INDEX items_key ON items (key);
  CREATE INDEX items_updated ON items (updated_at);
  CREATE TABLE edges (
    src INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    dst INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    rel TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (src, dst, rel)
  );
  CREATE INDEX edges_dst ON edges (dst);
  CREATE TABLE facts (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    value_json TEXT NOT NULL,
    source TEXT,
    provenance TEXT NOT NULL,
    at INTEGER NOT NULL
  );
  CREATE INDEX facts_item ON facts (item_id);
  CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    job_id TEXT NOT NULL,
    feature_id TEXT NOT NULL,
    scope_key TEXT,
    status TEXT NOT NULL,
    at INTEGER NOT NULL,
    duration_ms INTEGER,
    outcome TEXT NOT NULL,
    metrics_json TEXT,
    UNIQUE (job_id, status)
  );
  CREATE INDEX events_feature_at ON events (feature_id, at);
  CREATE VIRTUAL TABLE items_fts USING fts5 (title, excerpt);
  `,
  // v2 (Phase 8): one embedding per item, for similar-item search. The
  // vector is an L2-normalised Float32 BLOB (core/similar.js); deleting the
  // item (forget, retention) deletes it too.
  `
  CREATE TABLE item_vectors (
    item_id INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL,
    item_updated_at INTEGER NOT NULL,
    at INTEGER NOT NULL
  );
  CREATE INDEX item_vectors_model ON item_vectors (model, dim);
  `,
];

const CONTROL_RE = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g;

function clipExcerpt(text) {
  if (typeof text !== "string") return null;
  const clean = text.replace(CONTROL_RE, "");
  if (!clean.trim()) return null;
  return clean.length > EXCERPT_MAX ? clean.slice(0, EXCERPT_MAX) : clean;
}

function clipTitle(text) {
  if (typeof text !== "string") return null;
  const clean = text.replace(/[\u0000-\u001f\u007f]/g, " ").trim();
  if (!clean) return null;
  return clean.length > TITLE_MAX ? clean.slice(0, TITLE_MAX) : clean;
}

const ISSUE_KEY_RE = /^([A-Za-z][A-Za-z0-9]*)-([1-9]\d*)$/;
const PR_REF_RE = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)#([1-9]\d*)$/;

/** The stored form of a key a person typed: bare `PROJ-12` → `jira:PROJ-12`,
 * `ci/sample-app#7` → `bitbucket:CI/sample-app#7` or `github:ci/sample-app#7` (the same forms core/scope-key.js
 * uses), anything else already prefixed passes through. `null` if unusable. */
function normalizeKey(input) {
  if (typeof input !== "string") return null;
  const s = input.trim();
  if (!s || s.length > KEY_MAX || /[\u0000-\u001f\u007f]/.test(s)) return null;
  let m = ISSUE_KEY_RE.exec(s);
  if (m) return `jira:${m[1].toUpperCase()}-${m[2]}`;
  m = PR_REF_RE.exec(s);
  if (m) return require("./prereqs.js").gitRemoteRules().prKey(m[1], m[2], m[3]);
  return s;
}

function median(sorted) {
  if (sorted.length === 0) return null;
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : Math.round((sorted[mid - 1] + sorted[mid]) / 2);
}

/** `events` rows (`feature_id`, `outcome`, `duration_ms`) → one summary per
 * feature+outcome, sorted, with median and average duration (null when no
 * row carried a duration). */
function summarizeEvents(rows) {
  const groups = new Map();
  for (const row of rows) {
    const k = `${row.feature_id}\u0000${row.outcome}`;
    if (!groups.has(k)) groups.set(k, { featureId: row.feature_id, outcome: row.outcome, count: 0, durations: [] });
    const g = groups.get(k);
    g.count += 1;
    if (Number.isFinite(row.duration_ms)) g.durations.push(row.duration_ms);
  }
  return [...groups.values()]
    .map((g) => {
      const d = g.durations.slice().sort((a, b) => a - b);
      return {
        featureId: g.featureId,
        outcome: g.outcome,
        count: g.count,
        medianMs: median(d),
        avgMs: d.length ? Math.round(d.reduce((a, b) => a + b, 0) / d.length) : null,
      };
    })
    .sort((a, b) => a.featureId.localeCompare(b.featureId) || a.outcome.localeCompare(b.outcome));
}

/**
 * Pre-warmed runs per watcher: how many a watcher started and how many the
 * user actually used — approved (a conflict resolution pushed) or opened (a
 * background analysis read from the inbox). `rows` are events rows whose
 * metrics_json names a `watcher`; each job counts once. If a watcher's
 * results keep going unused, it isn't worth its Claude budget.
 */
function summarizePrewarmed(rows) {
  const jobs = new Map();
  for (const row of rows) {
    let metrics = null;
    try {
      metrics = row.metrics_json ? JSON.parse(row.metrics_json) : null;
    } catch {
      metrics = null;
    }
    if (!metrics || typeof metrics.watcher !== "string") continue;
    const job = jobs.get(row.job_id) || { watcher: metrics.watcher, outcomes: new Set() };
    job.outcomes.add(row.outcome);
    jobs.set(row.job_id, job);
  }
  const groups = new Map();
  for (const { watcher, outcomes } of jobs.values()) {
    const g = groups.get(watcher) || { watcher, runs: 0, used: 0, discarded: 0, failed: 0, expired: 0 };
    g.runs += 1;
    if (outcomes.has("approved") || outcomes.has("used")) g.used += 1;
    else if (outcomes.has("discarded")) g.discarded += 1;
    else if (outcomes.has("failed")) g.failed += 1;
    else if (outcomes.has("expired")) g.expired += 1;
    groups.set(watcher, g);
  }
  return [...groups.values()]
    .map((g) => ({ ...g, usedFraction: g.runs ? Math.round((g.used / g.runs) * 100) / 100 : null }))
    .sort((a, b) => a.watcher.localeCompare(b.watcher));
}

module.exports = {
  SCHEMA_VERSION,
  KINDS,
  RELS,
  PROVENANCE,
  OUTCOMES,
  MIGRATIONS,
  DEFAULT_RETENTION_DAYS,
  clipExcerpt,
  clipTitle,
  normalizeKey,
  summarizeEvents,
  summarizePrewarmed,
};
