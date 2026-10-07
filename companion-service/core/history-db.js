// The local history store: one SQLite file (Node's built-in node:sqlite) at
// stateDir()/history.db holding this user's own tickets, PRs, analyses,
// sessions and job timings — never credentials or cookies. Plain JS so
// node:test covers it; core/history.ts types it for the TypeScript side.
const fs = require("fs");
const path = require("path");
const schema = require("./history-schema.js");
const similar = require("./similar.js");

/** node:sqlite prints an ExperimentalWarning on some Node versions. Silence
 * that one warning while loading it — not warnings in general. */
function requireSqlite() {
  const original = process.emitWarning;
  process.emitWarning = function (warning, ...rest) {
    const message = typeof warning === "string" ? warning : warning && warning.message;
    const type = typeof rest[0] === "string" ? rest[0] : rest[0] && rest[0].type;
    if (type === "ExperimentalWarning" && /SQLite/i.test(String(message))) return;
    return original.call(process, warning, ...rest);
  };
  try {
    return require("node:sqlite");
  } finally {
    process.emitWarning = original;
  }
}

const DAY_MS = 24 * 60 * 60 * 1000;
const FACT_VALUE_MAX = 8192;
/** nearest() compares at most this many (newest) vectors: brute force stays bounded. */
const NEAREST_ROWS_MAX = 20000;

const nn = (v) => (v === undefined ? null : v);
const plain = (row) => ({ ...row });

function migrate(db) {
  const current = db.prepare("PRAGMA user_version").get().user_version;
  if (current > schema.SCHEMA_VERSION) {
    throw new Error(
      `history.db was written by a newer companion (schema ${current}); update the companion or move the file aside.`,
    );
  }
  for (let v = current; v < schema.MIGRATIONS.length; v++) {
    db.exec("BEGIN");
    try {
      db.exec(schema.MIGRATIONS[v]);
      db.exec(`PRAGMA user_version = ${v + 1}`);
      db.exec("COMMIT");
    } catch (err) {
      db.exec("ROLLBACK");
      throw err;
    }
  }
}

function ftsQuery(text) {
  const terms = String(text)
    .split(/\s+/)
    .map((t) => t.replace(/[^\p{L}\p{N}_-]/gu, ""))
    .filter((t) => /[\p{L}\p{N}]/u.test(t))
    .slice(0, 8);
  return terms.map((t) => `"${t}"*`).join(" ");
}

function openHistory(file, { now = Date.now } = {}) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  // Create it at 0600 first: SQLite gives its -wal/-shm files the main file's mode.
  fs.closeSync(fs.openSync(file, "a", 0o600));
  // Tighten an older, looser file (and its WAL files) too; chmod is a no-op-ish on Windows.
  for (const f of [file, `${file}-wal`, `${file}-shm`]) {
    try {
      if (fs.existsSync(f)) fs.chmodSync(f, 0o600);
    } catch {
      /* best effort */
    }
  }
  const { DatabaseSync } = requireSqlite();
  const db = new DatabaseSync(file);
  try {
    db.exec("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 3000;");
    migrate(db);
  } catch (err) {
    try {
      db.close();
    } catch {
      /* already closed */
    }
    throw err;
  }

  function refreshFts(id, title, excerpt) {
    db.prepare("DELETE FROM items_fts WHERE rowid = ?").run(id);
    db.prepare("INSERT INTO items_fts (rowid, title, excerpt) VALUES (?, ?, ?)").run(id, title || "", excerpt || "");
  }

  function upsertItem({ kind, key, repo, title, url, excerpt, data, at }) {
    if (!schema.KINDS.includes(kind)) throw new Error(`Unknown history kind: ${kind}`);
    const k = schema.normalizeKey(key);
    if (!k) throw new Error("Invalid history key.");
    const row = db
      .prepare(
        `INSERT INTO items (kind, key, repo, title, url, excerpt, updated_at, data_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (kind, key) DO UPDATE SET
           repo = COALESCE(excluded.repo, repo),
           title = COALESCE(excluded.title, title),
           url = COALESCE(excluded.url, url),
           excerpt = COALESCE(excluded.excerpt, excerpt),
           updated_at = excluded.updated_at,
           data_json = COALESCE(excluded.data_json, data_json)
         RETURNING id, title, excerpt`,
      )
      .get(
        kind,
        k,
        nn(repo),
        schema.clipTitle(title),
        nn(url),
        schema.clipExcerpt(excerpt),
        at ?? now(),
        data === undefined ? null : JSON.stringify(data),
      );
    refreshFts(row.id, row.title, row.excerpt);
    return row.id;
  }

  function addEdge({ src, dst, rel, at }) {
    if (!schema.RELS.includes(rel)) throw new Error(`Unknown history relation: ${rel}`);
    db.prepare(
      `INSERT INTO edges (src, dst, rel, at) VALUES (?, ?, ?, ?)
       ON CONFLICT (src, dst, rel) DO UPDATE SET at = excluded.at`,
    ).run(src, dst, rel, at ?? now());
  }

  function addFact({ itemId, kind, value, source, provenance, at }) {
    if (!schema.PROVENANCE.includes(provenance)) throw new Error(`Unknown fact provenance: ${provenance}`);
    if (typeof kind !== "string" || !kind || kind.length > 60) throw new Error("Invalid fact kind.");
    const json = JSON.stringify(value);
    if (json === undefined || json.length > FACT_VALUE_MAX) throw new Error("Fact value is missing or too large.");
    const r = db
      .prepare("INSERT INTO facts (item_id, kind, value_json, source, provenance, at) VALUES (?, ?, ?, ?, ?, ?)")
      .run(itemId, kind, json, nn(source), provenance, at ?? now());
    return Number(r.lastInsertRowid);
  }

  function recordEvent({ jobId, featureId, scopeKey, status, at, durationMs, outcome, metrics }) {
    if (!schema.OUTCOMES.includes(outcome)) throw new Error(`Unknown event outcome: ${outcome}`);
    db.prepare(
      `INSERT INTO events (job_id, feature_id, scope_key, status, at, duration_ms, outcome, metrics_json)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (job_id, status) DO UPDATE SET
         scope_key = excluded.scope_key, at = excluded.at, duration_ms = excluded.duration_ms,
         outcome = excluded.outcome, metrics_json = excluded.metrics_json`,
    ).run(
      jobId,
      featureId,
      nn(scopeKey),
      status,
      at ?? now(),
      Number.isFinite(durationMs) ? Math.round(durationMs) : null,
      outcome,
      metrics === undefined ? null : JSON.stringify(metrics),
    );
  }

  function getItem(keyInput) {
    const k = schema.normalizeKey(keyInput);
    if (!k) return null;
    const item = db.prepare("SELECT * FROM items WHERE key = ? ORDER BY updated_at DESC LIMIT 1").get(k);
    if (!item) return null;
    const out = db
      .prepare("SELECT e.rel, e.at, i.kind, i.key, i.title FROM edges e JOIN items i ON i.id = e.dst WHERE e.src = ?")
      .all(item.id)
      .map((r) => ({ ...plain(r), direction: "out" }));
    const inn = db
      .prepare("SELECT e.rel, e.at, i.kind, i.key, i.title FROM edges e JOIN items i ON i.id = e.src WHERE e.dst = ?")
      .all(item.id)
      .map((r) => ({ ...plain(r), direction: "in" }));
    const facts = db
      .prepare("SELECT id, kind, value_json, source, provenance, at FROM facts WHERE item_id = ? ORDER BY at DESC, id DESC LIMIT 50")
      .all(item.id)
      .map((f) => ({ id: f.id, kind: f.kind, value: JSON.parse(f.value_json), source: f.source, provenance: f.provenance, at: f.at }));
    return {
      item: {
        id: item.id,
        kind: item.kind,
        key: item.key,
        repo: item.repo,
        title: item.title,
        url: item.url,
        excerpt: item.excerpt,
        updatedAt: item.updated_at,
        data: item.data_json ? JSON.parse(item.data_json) : null,
      },
      edges: [...out, ...inn],
      facts,
    };
  }

  function search(query, limit = 20) {
    const q = ftsQuery(query);
    if (!q) return [];
    return db
      .prepare(
        `SELECT i.kind, i.key, i.title, i.updated_at AS updatedAt
         FROM items_fts JOIN items i ON i.id = items_fts.rowid
         WHERE items_fts MATCH ? ORDER BY rank LIMIT ?`,
      )
      .all(q, Math.max(1, Math.min(Number(limit) || 20, 100)))
      .map(plain);
  }

  function deleteItems(ids) {
    for (const id of ids) db.prepare("DELETE FROM items_fts WHERE rowid = ?").run(id);
    for (const id of ids) db.prepare("DELETE FROM items WHERE id = ?").run(id);
  }

  function forget(keyInput) {
    const k = schema.normalizeKey(keyInput);
    if (!k) return { items: 0, events: 0 };
    db.exec("BEGIN");
    try {
      const ids = db.prepare("SELECT id FROM items WHERE key = ?").all(k).map((r) => r.id);
      deleteItems(ids);
      const jobId = k.startsWith("job:") ? k.slice(4) : k;
      const events = db.prepare("DELETE FROM events WHERE scope_key = ? OR job_id = ?").run(k, jobId).changes;
      db.exec("COMMIT");
      return { items: ids.length, events: Number(events) };
    } catch (err) {
      db.exec("ROLLBACK");
      throw err;
    }
  }

  function prune(retentionDays, at) {
    const cutoff = (at ?? now()) - retentionDays * DAY_MS;
    db.exec("BEGIN");
    try {
      const ids = db.prepare("SELECT id FROM items WHERE updated_at < ?").all(cutoff).map((r) => r.id);
      deleteItems(ids);
      const events = db.prepare("DELETE FROM events WHERE at < ?").run(cutoff).changes;
      db.exec("COMMIT");
      return { items: ids.length, events: Number(events) };
    } catch (err) {
      db.exec("ROLLBACK");
      throw err;
    }
  }

  function metrics({ days = 30, at } = {}) {
    const since = (at ?? now()) - days * DAY_MS;
    const rows = db
      .prepare("SELECT feature_id, outcome, duration_ms FROM events WHERE at >= ?")
      .all(since)
      .map(plain);
    return schema.summarizeEvents(rows);
  }

  function prewarmMetrics({ days = 30, at } = {}) {
    const since = (at ?? now()) - days * DAY_MS;
    const rows = db
      .prepare("SELECT job_id, outcome, metrics_json FROM events WHERE at >= ? AND metrics_json LIKE '%\"watcher\"%'")
      .all(since)
      .map(plain);
    return schema.summarizePrewarmed(rows);
  }

  // ---- similar items (Phase 8): vectors and any-word text search ----

  /** Only known kinds reach the SQL, as bound parameters. */
  function kindList(kinds) {
    const list = (Array.isArray(kinds) ? kinds : similar.VECTOR_KINDS).filter((k) => schema.KINDS.includes(k));
    return list.length ? list : similar.VECTOR_KINDS;
  }

  const itemOut = (r) => ({ id: r.id, kind: r.kind, key: r.key, title: r.title, excerpt: r.excerpt, updatedAt: r.updated_at });

  /** Items with text that have no vector for `model` yet, or changed since
   * theirs was made; newest first. */
  function pendingEmbeddings({ model, kinds, limit = similar.EMBED_PER_RUN } = {}) {
    if (typeof model !== "string" || !model) return [];
    const list = kindList(kinds);
    return db
      .prepare(
        `SELECT i.id, i.kind, i.key, i.title, i.excerpt, i.updated_at FROM items i
         LEFT JOIN item_vectors v ON v.item_id = i.id
         WHERE i.kind IN (${list.map(() => "?").join(", ")})
           AND (i.title IS NOT NULL OR i.excerpt IS NOT NULL)
           AND (v.item_id IS NULL OR v.model != ? OR v.item_updated_at < i.updated_at)
         ORDER BY i.updated_at DESC, i.id DESC LIMIT ?`,
      )
      .all(...list, model, Math.max(1, Math.min(Number(limit) || 1, 500)))
      .map(itemOut);
  }

  /** Stores (or replaces) one item's vector: a list of numbers, normalised here. */
  function saveVector({ itemId, model, vector, itemUpdatedAt, at }) {
    if (typeof model !== "string" || !model || model.length > 100) throw new Error("Invalid embedding model name.");
    if (!Number.isInteger(itemId) || itemId < 1) throw new Error("Invalid item id.");
    if (!Number.isFinite(itemUpdatedAt)) throw new Error("Invalid item update time.");
    const blob = similar.encodeVector(vector);
    db.prepare(
      `INSERT INTO item_vectors (item_id, model, dim, vec, item_updated_at, at) VALUES (?, ?, ?, ?, ?, ?)
       ON CONFLICT (item_id) DO UPDATE SET model = excluded.model, dim = excluded.dim, vec = excluded.vec,
         item_updated_at = excluded.item_updated_at, at = excluded.at`,
    ).run(itemId, model, blob.byteLength / 4, blob, itemUpdatedAt, at ?? now());
  }

  /** One item's stored vector for `model`, or null. */
  function vectorFor(itemId, model) {
    const row = db.prepare("SELECT vec FROM item_vectors WHERE item_id = ? AND model = ?").get(itemId, model);
    return row ? similar.decodeVector(row.vec) : null;
  }

  /** The items nearest to `vector` (a normalised Float32Array) by cosine,
   * brute force over this user's own newest vectors of the same model and
   * size (at most NEAREST_ROWS_MAX), ignoring vectors older than their item. Only scores of at least `minScore` count. */
  function nearest({ model, vector, kinds, limit = 20, minScore = similar.MIN_COSINE } = {}) {
    if (!(vector instanceof Float32Array) || vector.length === 0) return [];
    const list = kindList(kinds);
    const stmt = db.prepare(
      `SELECT i.id, i.kind, i.key, i.title, i.excerpt, i.updated_at, v.vec FROM item_vectors v
       JOIN items i ON i.id = v.item_id
       WHERE v.model = ? AND v.dim = ? AND v.item_updated_at >= i.updated_at AND i.kind IN (${list.map(() => "?").join(", ")})
       ORDER BY i.updated_at DESC LIMIT ${NEAREST_ROWS_MAX}`,
    );
    const rows = typeof stmt.iterate === "function" ? stmt.iterate(model, vector.length, ...list) : stmt.all(model, vector.length, ...list);
    const hits = [];
    for (const r of rows) {
      const score = similar.cosine(vector, similar.decodeVector(r.vec));
      if (score >= minScore) hits.push({ ...itemOut(r), score: Math.round(score * 1e4) / 1e4 });
    }
    return hits.sort((a, b) => b.score - a.score || b.updatedAt - a.updatedAt).slice(0, Math.max(1, Math.min(Number(limit) || 20, 100)));
  }

  /** Items matching ANY of the text's words (see similar.ftsAnyQuery), best first. */
  function searchAny(text, { kinds, limit = 20 } = {}) {
    const q = similar.ftsAnyQuery(text);
    if (!q) return [];
    const list = kindList(kinds);
    return db
      .prepare(
        `SELECT i.id, i.kind, i.key, i.title, i.excerpt, i.updated_at FROM items_fts
         JOIN items i ON i.id = items_fts.rowid
         WHERE items_fts MATCH ? AND i.kind IN (${list.map(() => "?").join(", ")})
         ORDER BY rank LIMIT ?`,
      )
      .all(q, ...list, Math.max(1, Math.min(Number(limit) || 20, 100)))
      .map(itemOut);
  }

  /** Whether any real vector (not a placeholder for an item that couldn't be
   * embedded, which is one number long) is stored for `model`. */
  function hasVectors(model) {
    if (typeof model !== "string" || !model) return false;
    return Boolean(db.prepare("SELECT 1 AS x FROM item_vectors WHERE model = ? AND dim > 1 LIMIT 1").get(model));
  }

  /** How many items have a vector, per model — for doctor and the embed job. */
  function vectorStats() {
    const models = db
      .prepare("SELECT model, dim, COUNT(*) AS n FROM item_vectors GROUP BY model, dim ORDER BY n DESC")
      .all()
      .map((r) => ({ model: r.model, dim: r.dim, count: r.n }));
    const list = similar.VECTOR_KINDS;
    const embeddable = db
      .prepare(`SELECT COUNT(*) AS n FROM items WHERE kind IN (${list.map(() => "?").join(", ")}) AND (title IS NOT NULL OR excerpt IS NOT NULL)`)
      .get(...list).n;
    return { vectors: models.reduce((a, m) => a + m.count, 0), embeddable, models };
  }

  return {
    upsertItem,
    addEdge,
    addFact,
    recordEvent,
    getItem,
    search,
    forget,
    prune,
    metrics,
    prewarmMetrics,
    pendingEmbeddings,
    saveVector,
    vectorFor,
    nearest,
    searchAny,
    hasVectors,
    vectorStats,
    close: () => db.close(),
  };
}

module.exports = { openHistory };
