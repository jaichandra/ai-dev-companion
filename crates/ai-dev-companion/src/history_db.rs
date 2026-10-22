//! The local history store (port of `core/history-db.js`): one SQLite file at
//! `<stateDir>/history.db` holding this user's own tickets, PRs, analyses,
//! sessions and job timings, never credentials or cookies. SQLite is
//! `rusqlite` with the bundled library (FTS5 included), so nothing needs to be
//! installed on the machine.

use crate::history_record::{History, ItemOp};
use crate::history_schema as schema;
use crate::js_text::{is_js_space, utf16_len};
use crate::prereqs::GitRemoteRules;
use crate::similar;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Mutex;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const FACT_VALUE_MAX: usize = 8192;
/// `nearest` compares at most this many (newest) vectors: brute force stays bounded.
const NEAREST_ROWS_MAX: usize = 20_000;

fn e(err: rusqlite::Error) -> String {
    err.to_string()
}

/// An item as the similar-item search sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarItem {
    pub id: i64,
    pub kind: String,
    pub key: String,
    pub title: Option<String>,
    pub excerpt: Option<String>,
    pub updated_at: i64,
    /// Cosine similarity, for `nearest` hits only.
    pub score: Option<f64>,
}

impl SimilarItem {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "id": self.id, "kind": self.kind, "key": self.key, "title": self.title, "excerpt": self.excerpt, "updatedAt": self.updated_at });
        if let Some(s) = self.score {
            v["score"] = json!(s);
        }
        v
    }
}

/// What to store for an item. A field left `None` keeps the stored one.
#[derive(Debug, Default, Clone)]
pub struct UpsertItem<'a> {
    pub kind: &'a str,
    pub key: &'a str,
    pub repo: Option<&'a str>,
    pub title: Option<&'a str>,
    pub url: Option<&'a str>,
    pub excerpt: Option<&'a str>,
    pub data: Option<&'a Value>,
    pub at: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct EventInput<'a> {
    pub job_id: &'a str,
    pub feature_id: &'a str,
    pub scope_key: Option<&'a str>,
    pub status: &'a str,
    pub at: Option<i64>,
    pub duration_ms: Option<f64>,
    pub outcome: &'a str,
    pub metrics: Option<&'a Value>,
}

pub struct HistoryDb {
    conn: Mutex<Connection>,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    git: GitRemoteRules,
}

fn migrate(conn: &Connection) -> Result<(), String> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(e)?;
    if current > i64::from(schema::SCHEMA_VERSION) {
        return Err(format!("history.db was written by a newer companion (schema {current}); update the companion or move the file aside."));
    }
    for v in current as usize..schema::MIGRATIONS.len() {
        conn.execute_batch("BEGIN").map_err(e)?;
        let run = conn.execute_batch(schema::MIGRATIONS[v]).and_then(|()| conn.execute_batch(&format!("PRAGMA user_version = {}", v + 1)));
        match run {
            Ok(()) => conn.execute_batch("COMMIT").map_err(e)?,
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e(err));
            }
        }
    }
    Ok(())
}

/// An FTS5 prefix query over up to 8 of the text's words.
fn fts_query(text: &str) -> String {
    text.split(is_js_space)
        .map(|t| t.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect::<String>())
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .take(8)
        .map(|t| format!("\"{t}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn clamp_limit(limit: Option<i64>, default: i64, max: i64) -> i64 {
    limit.filter(|l| *l != 0).unwrap_or(default).clamp(1, max)
}

fn kind_list(kinds: Option<&[&str]>) -> Vec<String> {
    let list: Vec<String> = kinds.unwrap_or(&similar::VECTOR_KINDS).iter().filter(|k| schema::KINDS.contains(k)).map(|k| k.to_string()).collect();
    if list.is_empty() { similar::VECTOR_KINDS.iter().map(|k| k.to_string()).collect() } else { list }
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn similar_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<SimilarItem> {
    Ok(SimilarItem { id: r.get(0)?, kind: r.get(1)?, key: r.get(2)?, title: r.get(3)?, excerpt: r.get(4)?, updated_at: r.get(5)?, score: None })
}

impl HistoryDb {
    pub fn open(file: &Path, git: GitRemoteRules) -> Result<Self, String> {
        Self::open_with_clock(file, git, || crate::job_files::now_ms() as i64)
    }

    pub fn open_with_clock(file: &Path, git: GitRemoteRules, now: impl Fn() -> i64 + Send + Sync + 'static) -> Result<Self, String> {
        if let Some(dir) = file.parent() {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|x| x.to_string())?;
        }
        // Create it at 0600 first: SQLite gives its -wal/-shm files the main file's mode.
        fs::OpenOptions::new().append(true).create(true).mode(0o600).open(file).map_err(|x| x.to_string())?;
        // Tighten an older, looser file (and its WAL files) too.
        for suffix in ["", "-wal", "-shm"] {
            let f = format!("{}{suffix}", file.display());
            if Path::new(&f).exists() {
                let _ = fs::set_permissions(&f, fs::Permissions::from_mode(0o600));
            }
        }
        let conn = Connection::open(file).map_err(e)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 3000;").map_err(e)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn), now: Box::new(now), git })
    }

    fn at(&self, at: Option<i64>) -> i64 {
        at.unwrap_or_else(|| (self.now)())
    }

    fn refresh_fts(conn: &Connection, id: i64, title: Option<&str>, excerpt: Option<&str>) -> Result<(), String> {
        conn.execute("DELETE FROM items_fts WHERE rowid = ?", [id]).map_err(e)?;
        conn.execute("INSERT INTO items_fts (rowid, title, excerpt) VALUES (?, ?, ?)", params![id, title.unwrap_or(""), excerpt.unwrap_or("")]).map_err(e)?;
        Ok(())
    }

    pub fn upsert_item(&self, item: &UpsertItem<'_>) -> Result<i64, String> {
        if !schema::KINDS.contains(&item.kind) {
            return Err(format!("Unknown history kind: {}", item.kind));
        }
        let key = schema::normalize_key(Some(item.key), self.git).ok_or("Invalid history key.")?;
        let conn = self.conn.lock().unwrap();
        let (id, title, excerpt): (i64, Option<String>, Option<String>) = conn
            .query_row(
                "INSERT INTO items (kind, key, repo, title, url, excerpt, updated_at, data_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (kind, key) DO UPDATE SET
                   repo = COALESCE(excluded.repo, repo),
                   title = COALESCE(excluded.title, title),
                   url = COALESCE(excluded.url, url),
                   excerpt = COALESCE(excluded.excerpt, excerpt),
                   updated_at = excluded.updated_at,
                   data_json = COALESCE(excluded.data_json, data_json)
                 RETURNING id, title, excerpt",
                params![
                    item.kind,
                    key,
                    item.repo,
                    schema::clip_title(item.title),
                    item.url,
                    schema::clip_excerpt(item.excerpt),
                    self.at(item.at),
                    item.data.map(|d| d.to_string()),
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(e)?;
        Self::refresh_fts(&conn, id, title.as_deref(), excerpt.as_deref())?;
        Ok(id)
    }

    pub fn add_edge(&self, src: i64, dst: i64, rel: &str, at: Option<i64>) -> Result<(), String> {
        if !schema::RELS.contains(&rel) {
            return Err(format!("Unknown history relation: {rel}"));
        }
        self.conn
            .lock()
            .unwrap()
            .execute("INSERT INTO edges (src, dst, rel, at) VALUES (?, ?, ?, ?) ON CONFLICT (src, dst, rel) DO UPDATE SET at = excluded.at", params![src, dst, rel, self.at(at)])
            .map_err(e)?;
        Ok(())
    }

    pub fn add_fact(&self, item_id: i64, kind: &str, value: Option<&Value>, source: Option<&str>, provenance: &str, at: Option<i64>) -> Result<i64, String> {
        if !schema::PROVENANCE.contains(&provenance) {
            return Err(format!("Unknown fact provenance: {provenance}"));
        }
        if kind.is_empty() || utf16_len(kind) > 60 {
            return Err("Invalid fact kind.".into());
        }
        let json = value.map(Value::to_string).filter(|j| utf16_len(j) <= FACT_VALUE_MAX).ok_or("Fact value is missing or too large.")?;
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO facts (item_id, kind, value_json, source, provenance, at) VALUES (?, ?, ?, ?, ?, ?)", params![item_id, kind, json, source, provenance, self.at(at)]).map_err(e)?;
        Ok(conn.last_insert_rowid())
    }

    pub fn record_event(&self, ev: &EventInput<'_>) -> Result<(), String> {
        if !schema::OUTCOMES.contains(&ev.outcome) {
            return Err(format!("Unknown event outcome: {}", ev.outcome));
        }
        let duration = ev.duration_ms.filter(|d| d.is_finite()).map(|d| (d + 0.5).floor() as i64);
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO events (job_id, feature_id, scope_key, status, at, duration_ms, outcome, metrics_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (job_id, status) DO UPDATE SET
                   scope_key = excluded.scope_key, at = excluded.at, duration_ms = excluded.duration_ms,
                   outcome = excluded.outcome, metrics_json = excluded.metrics_json",
                params![ev.job_id, ev.feature_id, ev.scope_key, ev.status, self.at(ev.at), duration, ev.outcome, ev.metrics.map(Value::to_string)],
            )
            .map_err(e)?;
        Ok(())
    }

    /// `record_event` from the event JSON the recorder builds (`jobId`, `featureId`, ...).
    pub fn record_event_json(&self, ev: &Value) -> Result<(), String> {
        self.record_event(&EventInput {
            job_id: ev["jobId"].as_str().unwrap_or(""),
            feature_id: ev["featureId"].as_str().unwrap_or(""),
            scope_key: ev["scopeKey"].as_str(),
            status: ev["status"].as_str().unwrap_or(""),
            at: ev["at"].as_i64(),
            duration_ms: ev["durationMs"].as_f64(),
            outcome: ev["outcome"].as_str().unwrap_or(""),
            metrics: ev.get("metrics"),
        })
    }

    /// The item for a key (as typed or stored) with its edges (both ways) and facts, or `None`.
    pub fn get_item(&self, key_input: &str) -> Result<Option<Value>, String> {
        let Some(key) = schema::normalize_key(Some(key_input), self.git) else { return Ok(None) };
        let conn = self.conn.lock().unwrap();
        let item = conn
            .query_row("SELECT id, kind, key, repo, title, url, excerpt, updated_at, data_json FROM items WHERE key = ? ORDER BY updated_at DESC LIMIT 1", [&key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, i64>(7)?, r.get::<_, Option<String>>(8)?))
            })
            .optional()
            .map_err(e)?;
        let Some((id, kind, key, repo, title, url, excerpt, updated_at, data_json)) = item else { return Ok(None) };
        let edges_of = |sql: &str, direction: &str| -> Result<Vec<Value>, String> {
            let mut stmt = conn.prepare(sql).map_err(e)?;
            let rows = stmt
                .query_map([id], |r| Ok(json!({ "rel": r.get::<_, String>(0)?, "at": r.get::<_, i64>(1)?, "kind": r.get::<_, String>(2)?, "key": r.get::<_, String>(3)?, "title": r.get::<_, Option<String>>(4)?, "direction": direction })))
                .map_err(e)?;
            rows.collect::<Result<_, _>>().map_err(e)
        };
        let mut edges = edges_of("SELECT e.rel, e.at, i.kind, i.key, i.title FROM edges e JOIN items i ON i.id = e.dst WHERE e.src = ?", "out")?;
        edges.extend(edges_of("SELECT e.rel, e.at, i.kind, i.key, i.title FROM edges e JOIN items i ON i.id = e.src WHERE e.dst = ?", "in")?);
        let mut stmt = conn.prepare("SELECT id, kind, value_json, source, provenance, at FROM facts WHERE item_id = ? ORDER BY at DESC, id DESC LIMIT 50").map_err(e)?;
        let facts: Vec<Value> = stmt
            .query_map([id], |r| {
                let raw: String = r.get(2)?;
                Ok(json!({ "id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "value": serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null), "source": r.get::<_, Option<String>>(3)?, "provenance": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)? }))
            })
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        let data = data_json.filter(|d| !d.is_empty()).and_then(|d| serde_json::from_str::<Value>(&d).ok()).unwrap_or(Value::Null);
        Ok(Some(json!({
            "item": { "id": id, "kind": kind, "key": key, "repo": repo, "title": title, "url": url, "excerpt": excerpt, "updatedAt": updated_at, "data": data },
            "edges": edges,
            "facts": facts,
        })))
    }

    /// Items matching the words of `query` (each as a prefix), best first.
    pub fn search(&self, query: &str, limit: Option<i64>) -> Result<Vec<Value>, String> {
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT i.kind, i.key, i.title, i.updated_at FROM items_fts JOIN items i ON i.id = items_fts.rowid WHERE items_fts MATCH ? ORDER BY rank LIMIT ?")
            .map_err(e)?;
        let rows = stmt
            .query_map(params![q, clamp_limit(limit, 20, 100)], |r| Ok(json!({ "kind": r.get::<_, String>(0)?, "key": r.get::<_, String>(1)?, "title": r.get::<_, Option<String>>(2)?, "updatedAt": r.get::<_, i64>(3)? })))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn delete_items(conn: &Connection, ids: &[i64]) -> Result<(), String> {
        for id in ids {
            conn.execute("DELETE FROM items_fts WHERE rowid = ?", [id]).map_err(e)?;
        }
        for id in ids {
            conn.execute("DELETE FROM items WHERE id = ?", [id]).map_err(e)?;
        }
        Ok(())
    }

    fn in_transaction<T>(conn: &Connection, work: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        conn.execute_batch("BEGIN").map_err(e)?;
        match work(conn) {
            Ok(v) => {
                conn.execute_batch("COMMIT").map_err(e)?;
                Ok(v)
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    fn ids(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<i64>, String> {
        let mut stmt = conn.prepare(sql).map_err(e)?;
        let rows = stmt.query_map(p, |r| r.get(0)).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Removes the item (its edges, facts, vector and search hit) and its events.
    pub fn forget(&self, key_input: &str) -> Result<(usize, usize), String> {
        let Some(key) = schema::normalize_key(Some(key_input), self.git) else { return Ok((0, 0)) };
        let conn = self.conn.lock().unwrap();
        Self::in_transaction(&conn, |c| {
            let ids = Self::ids(c, "SELECT id FROM items WHERE key = ?", [&key])?;
            Self::delete_items(c, &ids)?;
            let job_id = key.strip_prefix("job:").unwrap_or(&key);
            let events = c.execute("DELETE FROM events WHERE scope_key = ? OR job_id = ?", params![key, job_id]).map_err(e)?;
            Ok((ids.len(), events))
        })
    }

    /// Drops items and events older than the retention window.
    pub fn prune(&self, retention_days: i64, at: Option<i64>) -> Result<(usize, usize), String> {
        let cutoff = self.at(at) - retention_days * DAY_MS;
        let conn = self.conn.lock().unwrap();
        Self::in_transaction(&conn, |c| {
            let ids = Self::ids(c, "SELECT id FROM items WHERE updated_at < ?", [cutoff])?;
            Self::delete_items(c, &ids)?;
            let events = c.execute("DELETE FROM events WHERE at < ?", [cutoff]).map_err(e)?;
            Ok((ids.len(), events))
        })
    }

    pub fn metrics(&self, days: i64, at: Option<i64>) -> Result<Vec<Value>, String> {
        let since = self.at(at) - days * DAY_MS;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT feature_id, outcome, duration_ms FROM events WHERE at >= ?").map_err(e)?;
        let rows: Vec<Value> = stmt
            .query_map([since], |r| Ok(json!({ "feature_id": r.get::<_, String>(0)?, "outcome": r.get::<_, String>(1)?, "duration_ms": r.get::<_, Option<i64>>(2)? })))
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        Ok(schema::summarize_events(&rows))
    }

    pub fn prewarm_metrics(&self, days: i64, at: Option<i64>) -> Result<Vec<Value>, String> {
        let since = self.at(at) - days * DAY_MS;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT job_id, outcome, metrics_json FROM events WHERE at >= ? AND metrics_json LIKE '%\"watcher\"%'").map_err(e)?;
        let rows: Vec<Value> = stmt
            .query_map([since], |r| Ok(json!({ "job_id": r.get::<_, String>(0)?, "outcome": r.get::<_, String>(1)?, "metrics_json": r.get::<_, Option<String>>(2)? })))
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        Ok(schema::summarize_prewarmed(&rows))
    }

    // ---- similar items: vectors and any-word text search ----

    /// Items with text that have no vector for `model` yet, or changed since
    /// theirs was made; newest first.
    pub fn pending_embeddings(&self, model: &str, kinds: Option<&[&str]>, limit: Option<i64>) -> Result<Vec<SimilarItem>, String> {
        if model.is_empty() {
            return Ok(vec![]);
        }
        let list = kind_list(kinds);
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT i.id, i.kind, i.key, i.title, i.excerpt, i.updated_at FROM items i
             LEFT JOIN item_vectors v ON v.item_id = i.id
             WHERE i.kind IN ({}) AND (i.title IS NOT NULL OR i.excerpt IS NOT NULL)
               AND (v.item_id IS NULL OR v.model != ? OR v.item_updated_at < i.updated_at)
             ORDER BY i.updated_at DESC, i.id DESC LIMIT ?",
            placeholders(list.len())
        );
        let mut stmt = conn.prepare(&sql).map_err(e)?;
        let mut args: Vec<rusqlite::types::Value> = list.into_iter().map(Into::into).collect();
        args.push(model.to_string().into());
        args.push(clamp_limit(limit.or(Some(similar::EMBED_PER_RUN as i64)), 1, 500).into());
        let rows = stmt.query_map(params_from_iter(args), similar_item).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Stores (or replaces) one item's vector: a list of numbers, normalised here.
    pub fn save_vector(&self, item_id: i64, model: &str, vector: &[f64], item_updated_at: i64, at: Option<i64>) -> Result<(), String> {
        if model.is_empty() || utf16_len(model) > 100 {
            return Err("Invalid embedding model name.".into());
        }
        if item_id < 1 {
            return Err("Invalid item id.".into());
        }
        let blob = similar::encode_vector(vector)?;
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO item_vectors (item_id, model, dim, vec, item_updated_at, at) VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (item_id) DO UPDATE SET model = excluded.model, dim = excluded.dim, vec = excluded.vec,
                   item_updated_at = excluded.item_updated_at, at = excluded.at",
                params![item_id, model, (blob.len() / 4) as i64, blob, item_updated_at, self.at(at)],
            )
            .map_err(e)?;
        Ok(())
    }

    /// One item's stored vector for `model`, or `None` (also when the stored bytes are corrupt).
    pub fn vector_for(&self, item_id: i64, model: &str) -> Result<Option<Vec<f32>>, String> {
        let blob: Option<Vec<u8>> = self.conn.lock().unwrap().query_row("SELECT vec FROM item_vectors WHERE item_id = ? AND model = ?", params![item_id, model], |r| r.get(0)).optional().map_err(e)?;
        Ok(blob.and_then(|b| similar::decode_vector(&b)))
    }

    /// The items nearest to `vector` (normalised) by cosine: brute force over
    /// this user's own newest vectors of the same model and size (at most
    /// `NEAREST_ROWS_MAX`), ignoring vectors older than their item. Only scores
    /// of at least `min_score` count.
    pub fn nearest(&self, model: &str, vector: &[f32], kinds: Option<&[&str]>, limit: Option<i64>, min_score: f64) -> Result<Vec<SimilarItem>, String> {
        if vector.is_empty() {
            return Ok(vec![]);
        }
        let list = kind_list(kinds);
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT i.id, i.kind, i.key, i.title, i.excerpt, i.updated_at, v.vec FROM item_vectors v
             JOIN items i ON i.id = v.item_id
             WHERE v.model = ? AND v.dim = ? AND v.item_updated_at >= i.updated_at AND i.kind IN ({})
             ORDER BY i.updated_at DESC LIMIT {NEAREST_ROWS_MAX}",
            placeholders(list.len())
        );
        let mut stmt = conn.prepare(&sql).map_err(e)?;
        let mut args: Vec<rusqlite::types::Value> = vec![model.to_string().into(), (vector.len() as i64).into()];
        args.extend(list.into_iter().map(rusqlite::types::Value::from));
        let rows = stmt.query_map(params_from_iter(args), |r| Ok((similar_item(r)?, r.get::<_, Vec<u8>>(6)?))).map_err(e)?;
        let mut hits = vec![];
        for row in rows {
            let (mut item, blob) = row.map_err(e)?;
            let score = similar::cosine(vector, &similar::decode_vector(&blob).unwrap_or_default());
            if score >= min_score {
                item.score = Some((score * 1e4).round() / 1e4);
                hits.push(item);
            }
        }
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(b.updated_at.cmp(&a.updated_at)));
        hits.truncate(clamp_limit(limit, 20, 100) as usize);
        Ok(hits)
    }

    /// Items matching ANY of the text's words (see `similar::fts_any_query`), best first.
    pub fn search_any(&self, text: &str, kinds: Option<&[&str]>, limit: Option<i64>) -> Result<Vec<SimilarItem>, String> {
        let q = similar::fts_any_query(text);
        if q.is_empty() {
            return Ok(vec![]);
        }
        let list = kind_list(kinds);
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT i.id, i.kind, i.key, i.title, i.excerpt, i.updated_at FROM items_fts
             JOIN items i ON i.id = items_fts.rowid
             WHERE items_fts MATCH ? AND i.kind IN ({}) ORDER BY rank LIMIT ?",
            placeholders(list.len())
        );
        let mut stmt = conn.prepare(&sql).map_err(e)?;
        let mut args: Vec<rusqlite::types::Value> = vec![q.into()];
        args.extend(list.into_iter().map(rusqlite::types::Value::from));
        args.push(clamp_limit(limit, 20, 100).into());
        let rows = stmt.query_map(params_from_iter(args), similar_item).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    /// Whether any real vector (not a placeholder for an item that couldn't be
    /// embedded, which is one number long) is stored for `model`.
    pub fn has_vectors(&self, model: &str) -> Result<bool, String> {
        if model.is_empty() {
            return Ok(false);
        }
        let hit: Option<i64> = self.conn.lock().unwrap().query_row("SELECT 1 FROM item_vectors WHERE model = ? AND dim > 1 LIMIT 1", [model], |r| r.get(0)).optional().map_err(e)?;
        Ok(hit.is_some())
    }

    /// How many items have a vector, per model: for doctor and the embed job.
    pub fn vector_stats(&self) -> Result<Value, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT model, dim, COUNT(*) AS n FROM item_vectors GROUP BY model, dim ORDER BY n DESC").map_err(e)?;
        let models: Vec<Value> = stmt
            .query_map([], |r| Ok(json!({ "model": r.get::<_, String>(0)?, "dim": r.get::<_, i64>(1)?, "count": r.get::<_, i64>(2)? })))
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        let list = kind_list(None);
        let embeddable: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM items WHERE kind IN ({}) AND (title IS NOT NULL OR excerpt IS NOT NULL)", placeholders(list.len())), params_from_iter(list), |r| r.get(0))
            .map_err(e)?;
        let vectors: i64 = models.iter().map(|m| m["count"].as_i64().unwrap_or(0)).sum();
        Ok(json!({ "vectors": vectors, "embeddable": embeddable, "models": models }))
    }
}

impl History for HistoryDb {
    fn upsert_item(&self, item: &ItemOp) -> Result<i64, String> {
        HistoryDb::upsert_item(
            self,
            &UpsertItem {
                kind: item.kind,
                key: &item.key,
                repo: item.repo.as_deref(),
                title: item.title.as_deref(),
                url: item.url.as_deref(),
                excerpt: item.excerpt.as_deref(),
                data: item.data.as_ref(),
                at: Some(item.at),
            },
        )
    }

    fn add_edge(&self, src: i64, dst: i64, rel: &str, at: i64) -> Result<(), String> {
        HistoryDb::add_edge(self, src, dst, rel, Some(at))
    }

    fn record_event(&self, event: &Value) -> Result<(), String> {
        self.record_event_json(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_record::{ops_for_job, used_event, HistoryRules, Recorder};
    use std::sync::Arc;

    const MODEL: &str = "embedding-model";
    const DAY: i64 = DAY_MS;
    use GitRemoteRules::Github;

    struct Fixture {
        h: HistoryDb,
        file: std::path::PathBuf,
        dir: tempfile::TempDir,
    }

    fn open() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("history.db");
        let h = HistoryDb::open(&file, Github).unwrap();
        Fixture { h, file, dir }
    }

    fn item<'a>(kind: &'a str, key: &'a str, title: Option<&'a str>, at: i64) -> UpsertItem<'a> {
        UpsertItem { kind, key, title, at: Some(at), ..Default::default() }
    }

    fn up(h: &HistoryDb, kind: &str, key: &str, title: &str, at: i64) -> i64 {
        h.upsert_item(&item(kind, key, Some(title), at)).unwrap()
    }

    fn keys(rows: &[Value]) -> Vec<String> {
        rows.iter().map(|r| r["key"].as_str().unwrap().to_string()).collect()
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn opens_the_database_and_its_wal_files_at_mode_0600() {
        let f = open();
        up(&f.h, "ticket", "PROJ-1", "x", 1);
        let nested = f.dir.path().join("a").join("b").join("history.db");
        HistoryDb::open(&nested, Github).unwrap();
        assert_eq!(mode(nested.parent().unwrap()), 0o700, "a directory it creates is private");
        let names: Vec<_> = fs::read_dir(f.dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("history.db")).collect();
        assert!(names.len() >= 2, "{names:?}");
        for n in names {
            assert_eq!(mode(&f.dir.path().join(&n)), 0o600, "{n}");
        }
    }

    #[test]
    fn upsert_item_keeps_the_same_id_and_earlier_fields_when_a_later_call_omits_them() {
        let f = open();
        let a = f.h.upsert_item(&UpsertItem { kind: "ticket", key: "PROJ-1", title: Some("First title"), repo: Some("CI/sample-app"), ..Default::default() }).unwrap();
        let b = f.h.upsert_item(&UpsertItem { kind: "ticket", key: "proj-1", excerpt: Some("later excerpt"), ..Default::default() }).unwrap();
        assert_eq!(a, b);
        let got = f.h.get_item("PROJ-1").unwrap().unwrap();
        assert_eq!((got["item"]["title"].as_str(), got["item"]["repo"].as_str(), got["item"]["excerpt"].as_str(), got["item"]["key"].as_str()), (Some("First title"), Some("CI/sample-app"), Some("later excerpt"), Some("jira:PROJ-1")));
    }

    #[test]
    fn get_item_returns_edges_in_both_directions_and_facts_with_parsed_values() {
        let f = open();
        let ticket = up(&f.h, "ticket", "PROJ-1", "Login fails", 1);
        let analysis = up(&f.h, "analysis", "analysis:jira:PROJ-1", "Analysis", 1);
        let job = up(&f.h, "job", "job:abc", "analyze", 1);
        f.h.add_edge(ticket, analysis, "analyzed", None).unwrap();
        f.h.add_edge(job, ticket, "links", None).unwrap();
        f.h.add_fact(ticket, "note", Some(&json!({ "severity": "high" })), None, "user", None).unwrap();
        let got = f.h.get_item("PROJ-1").unwrap().unwrap();
        let mut edges: Vec<String> = got["edges"].as_array().unwrap().iter().map(|e| format!("{}:{}:{}", e["direction"].as_str().unwrap(), e["rel"].as_str().unwrap(), e["key"].as_str().unwrap())).collect();
        edges.sort();
        assert_eq!(edges, ["in:links:job:abc", "out:analyzed:analysis:jira:PROJ-1"]);
        assert_eq!(got["facts"][0]["kind"], "note");
        assert_eq!(got["facts"][0]["value"], json!({ "severity": "high" }));
        assert_eq!(got["facts"][0]["provenance"], "user");
        assert_eq!(f.h.get_item("PROJ-999").unwrap(), None);
        assert_eq!(f.h.get_item("bad\nkey").unwrap(), None);
    }

    #[test]
    fn search_finds_items_by_title_and_excerpt_words_and_survives_hostile_input() {
        let f = open();
        f.h.upsert_item(&UpsertItem { kind: "ticket", key: "PROJ-1", title: Some("Login page times out"), excerpt: Some("flaky testcafe"), ..Default::default() }).unwrap();
        up(&f.h, "ticket", "PROJ-2", "Billing export", 1);
        assert_eq!(keys(&f.h.search("login", None).unwrap()), ["jira:PROJ-1"]);
        assert_eq!(keys(&f.h.search("testcafe", None).unwrap()), ["jira:PROJ-1"]);
        assert_eq!(keys(&f.h.search("logi", None).unwrap()), ["jira:PROJ-1"], "words match as prefixes");
        assert!(f.h.search("\"; DROP TABLE items; --", None).unwrap().is_empty());
        assert!(f.h.search("   ", None).unwrap().is_empty());
        assert_eq!(f.h.get_item("PROJ-2").unwrap().unwrap()["item"]["title"], "Billing export");
    }

    #[test]
    fn forget_removes_the_item_its_edges_facts_search_hit_and_its_events() {
        let f = open();
        let ticket = up(&f.h, "ticket", "PROJ-1", "Login", 1);
        let job = up(&f.h, "job", "job:j1", "job", 1);
        f.h.add_edge(job, ticket, "links", None).unwrap();
        f.h.add_fact(ticket, "note", Some(&json!(1)), None, "user", None).unwrap();
        f.h.record_event(&EventInput { job_id: "j1", feature_id: "analyze-issue", scope_key: Some("jira:PROJ-1"), status: "awaiting-approval", outcome: "completed", ..Default::default() }).unwrap();
        assert_eq!(f.h.forget("PROJ-1").unwrap(), (1, 1));
        assert_eq!(f.h.get_item("PROJ-1").unwrap(), None);
        assert!(f.h.search("login", None).unwrap().is_empty());
        assert_eq!(f.h.get_item("job:j1").unwrap().unwrap()["edges"], json!([]));
        assert_eq!(f.h.forget("PROJ-1").unwrap(), (0, 0));
    }

    #[test]
    fn prune_drops_only_items_and_events_older_than_the_retention_window() {
        let f = open();
        up(&f.h, "ticket", "PROJ-1", "old", 1000);
        up(&f.h, "ticket", "PROJ-2", "new", 200 * DAY);
        for (job, at) in [("a", 1000), ("b", 200 * DAY)] {
            f.h.record_event(&EventInput { job_id: job, feature_id: "f", status: "failed", outcome: "failed", at: Some(at), ..Default::default() }).unwrap();
        }
        assert_eq!(f.h.prune(180, Some(201 * DAY)).unwrap(), (1, 1));
        assert_eq!(f.h.get_item("PROJ-1").unwrap(), None);
        assert!(f.h.get_item("PROJ-2").unwrap().is_some());
    }

    #[test]
    fn metrics_summarises_events_inside_the_window_and_record_event_is_idempotent_per_job_and_status() {
        let f = open();
        let ev = |job: &'static str, at: i64, ms: f64| EventInput { job_id: job, feature_id: "resolve-conflict", status: "approved", outcome: "approved", at: Some(at), duration_ms: Some(ms), ..Default::default() };
        f.h.record_event(&ev("a", 40 * DAY, 100.0)).unwrap();
        f.h.record_event(&ev("a", 40 * DAY, 300.0)).unwrap();
        f.h.record_event(&ev("b", 1, 999.0)).unwrap();
        assert_eq!(f.h.metrics(30, Some(41 * DAY)).unwrap(), [json!({ "featureId": "resolve-conflict", "outcome": "approved", "count": 1, "medianMs": 300, "avgMs": 300 })]);
    }

    #[test]
    fn rejects_unknown_kinds_relations_provenance_and_outcomes() {
        let f = open();
        assert!(f.h.upsert_item(&item("nope", "x", None, 1)).unwrap_err().contains("kind"));
        assert!(f.h.upsert_item(&item("ticket", "", None, 1)).unwrap_err().contains("key"));
        let id = up(&f.h, "ticket", "PROJ-1", "t", 1);
        assert!(f.h.add_edge(id, id, "nope", None).unwrap_err().contains("relation"));
        assert!(f.h.add_fact(id, "n", Some(&json!(1)), None, "nope", None).unwrap_err().contains("provenance"));
        assert!(f.h.add_fact(id, "", Some(&json!(1)), None, "user", None).unwrap_err().contains("fact kind"));
        assert!(f.h.add_fact(id, "n", None, None, "user", None).unwrap_err().contains("too large"));
        assert!(f.h.add_fact(id, "n", Some(&json!("x".repeat(9000))), None, "user", None).unwrap_err().contains("too large"));
        assert!(f.h.record_event(&EventInput { job_id: "a", feature_id: "f", status: "s", outcome: "nope", ..Default::default() }).unwrap_err().contains("outcome"));
    }

    #[test]
    fn refuses_a_database_written_by_a_newer_schema_and_tightens_a_looser_file() {
        let f = open();
        drop(f.h);
        Connection::open(&f.file).unwrap().execute_batch("PRAGMA user_version = 99").unwrap();
        let err = HistoryDb::open(&f.file, Github).err().unwrap();
        assert!(err.contains("newer companion"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("history.db");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let h = HistoryDb::open(&file, Github).unwrap();
        assert_eq!(mode(&file), 0o600);
        up(&h, "ticket", "PROJ-1", "t", 1);
        assert_eq!(h.get_item("PROJ-1").unwrap().unwrap()["item"]["key"], "jira:PROJ-1");
    }

    // ---- vectors ----

    #[test]
    fn an_existing_v1_database_is_migrated_to_v2_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("history.db");
        {
            let raw = Connection::open(&file).unwrap();
            raw.execute_batch(schema::MIGRATIONS[0]).unwrap();
            raw.execute_batch("PRAGMA user_version = 1").unwrap();
            raw.execute("INSERT INTO items (kind, key, title, updated_at) VALUES ('ticket', 'jira:PROJ-1', 'Old ticket', 1)", []).unwrap();
        }
        let h = HistoryDb::open(&file, Github).unwrap();
        assert_eq!(h.get_item("PROJ-1").unwrap().unwrap()["item"]["title"], "Old ticket");
        assert_eq!(h.vector_stats().unwrap(), json!({ "vectors": 0, "embeddable": 1, "models": [] }));
        let v: i64 = Connection::open(&file).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, i64::from(schema::SCHEMA_VERSION));
    }

    fn ids(items: Vec<SimilarItem>) -> Vec<i64> {
        items.into_iter().map(|i| i.id).collect()
    }

    #[test]
    fn pending_embeddings_lists_items_with_text_and_no_vector_or_a_stale_one_newest_first() {
        let f = open();
        let h = &f.h;
        let a = up(h, "ticket", "PROJ-1", "Login fails", 10);
        let b = h.upsert_item(&UpsertItem { kind: "analysis", key: "analysis:jira:PROJ-1", title: Some("Analysis of PROJ-1"), excerpt: Some("Null check"), at: Some(20), ..Default::default() }).unwrap();
        up(h, "job", "job:1", "a job", 30);
        h.upsert_item(&UpsertItem { kind: "pr", key: "CI/sample-app#3", at: Some(40), ..Default::default() }).unwrap();
        assert_eq!(ids(h.pending_embeddings(MODEL, None, None).unwrap()), [b, a], "jobs and text-less items are skipped");
        h.save_vector(a, MODEL, &[1.0, 0.0], 10, None).unwrap();
        assert_eq!(ids(h.pending_embeddings(MODEL, None, None).unwrap()), [b]);
        assert_eq!(ids(h.pending_embeddings("other-model", None, None).unwrap()), [b, a], "another model re-embeds");
        up(h, "ticket", "PROJ-1", "Login fails on SSO", 50);
        assert_eq!(ids(h.pending_embeddings(MODEL, None, None).unwrap()), [a, b], "an updated item is pending again");
        assert_eq!(ids(h.pending_embeddings(MODEL, None, Some(1)).unwrap()), [a]);
        assert!(h.pending_embeddings("", None, None).unwrap().is_empty());
    }

    #[test]
    fn nearest_ranks_by_cosine_within_one_model_and_size_above_the_threshold() {
        let f = open();
        let h = &f.h;
        let ids: Vec<i64> = ["PROJ-1", "PROJ-2", "PROJ-3", "PROJ-4"].iter().enumerate().map(|(i, k)| up(h, "ticket", k, k, i as i64 + 1)).collect();
        h.save_vector(ids[0], MODEL, &[1.0, 0.0, 0.0], 1, None).unwrap();
        h.save_vector(ids[1], MODEL, &[0.9, 0.1, 0.0], 2, None).unwrap();
        h.save_vector(ids[2], MODEL, &[0.0, 1.0, 0.0], 3, None).unwrap();
        h.save_vector(ids[3], "other", &[1.0, 0.0, 0.0], 4, None).unwrap();
        let q = h.vector_for(ids[0], MODEL).unwrap().unwrap();
        assert_eq!(q.len(), 3);
        let hits = h.nearest(MODEL, &q, None, None, similar::MIN_COSINE).unwrap();
        assert_eq!(hits.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["jira:PROJ-1", "jira:PROJ-2"], "PROJ-3 is below the threshold, PROJ-4 another model");
        assert_eq!(hits[0].score, Some(1.0));
        assert!(h.nearest(MODEL, &[1.0, 0.0], None, None, similar::MIN_COSINE).unwrap().is_empty(), "another size never matches");
        assert!(h.nearest(MODEL, &q, Some(&["analysis"]), None, similar::MIN_COSINE).unwrap().is_empty());
        assert_eq!(h.vector_for(ids[0], "other").unwrap(), None);
        assert_eq!(h.vector_stats().unwrap()["models"], json!([{ "model": MODEL, "dim": 3, "count": 3 }, { "model": "other", "dim": 3, "count": 1 }]));
        assert!(h.save_vector(ids[0], MODEL, &[0.0, 0.0, 0.0], 1, None).unwrap_err().contains("zeros"));
        assert!(h.nearest(MODEL, &[], None, None, 0.0).unwrap().is_empty());
    }

    #[test]
    fn search_any_matches_any_word_only_in_the_similar_item_kinds() {
        let f = open();
        let h = &f.h;
        up(h, "ticket", "PROJ-1", "Login page fails for SSO users", 1);
        h.upsert_item(&UpsertItem { kind: "analysis", key: "analysis:jira:PROJ-2", title: Some("Analysis of PROJ-2"), excerpt: Some("The SSO token refresh races"), ..Default::default() }).unwrap();
        up(h, "job", "job:1", "SSO job", 1);
        let mut found: Vec<String> = h.search_any("Users can't sign in with SSO after the upgrade", None, None).unwrap().into_iter().map(|i| i.key).collect();
        found.sort();
        assert_eq!(found, ["analysis:jira:PROJ-2", "jira:PROJ-1"]);
        assert!(h.search_any("the and of", None, None).unwrap().is_empty());
        assert!(h.search_any("\") OR 1=1 --", None, None).unwrap().is_empty());
    }

    #[test]
    fn forget_and_retention_delete_an_items_vector_with_it() {
        let f = open();
        let h = &f.h;
        let a = up(h, "ticket", "PROJ-1", "one", 1);
        let b = up(h, "ticket", "PROJ-2", "two", 40 * DAY);
        h.save_vector(a, MODEL, &[1.0, 0.0], 1, None).unwrap();
        h.save_vector(b, MODEL, &[0.0, 1.0], 2, None).unwrap();
        assert_eq!(h.vector_stats().unwrap()["vectors"], 2);
        h.prune(30, Some(50 * DAY)).unwrap();
        assert_eq!(h.vector_stats().unwrap()["vectors"], 1, "the pruned item's vector is gone");
        assert_eq!(h.vector_for(a, MODEL).unwrap(), None);
        h.forget("PROJ-2").unwrap();
        assert_eq!(h.vector_stats().unwrap()["vectors"], 0, "the forgotten item's vector is gone");
    }

    #[test]
    fn nearest_ignores_a_vector_older_than_its_item_and_save_vector_validates_its_ids() {
        let f = open();
        let h = &f.h;
        let a = up(h, "ticket", "PROJ-1", "one", 1);
        h.save_vector(a, MODEL, &[1.0, 0.0], 1, None).unwrap();
        let q = h.vector_for(a, MODEL).unwrap().unwrap();
        assert_eq!(h.nearest(MODEL, &q, None, None, similar::MIN_COSINE).unwrap().len(), 1);
        up(h, "ticket", "PROJ-1", "one, rewritten", 5);
        assert_eq!(h.nearest(MODEL, &q, None, None, similar::MIN_COSINE).unwrap().len(), 0, "a stale vector must not rank");
        h.save_vector(a, MODEL, &[1.0, 0.0], 5, None).unwrap();
        assert_eq!(h.nearest(MODEL, &q, None, None, similar::MIN_COSINE).unwrap().len(), 1);
        assert!(h.save_vector(0, MODEL, &[1.0, 0.0], 1, None).unwrap_err().contains("Invalid"));
        assert!(h.save_vector(a, "", &[1.0, 0.0], 1, None).unwrap_err().contains("Invalid"));
        assert!(h.save_vector(a, &"m".repeat(101), &[1.0, 0.0], 1, None).unwrap_err().contains("Invalid"));
    }

    #[test]
    fn a_corrupt_stored_blob_is_skipped_not_thrown_on() {
        let f = open();
        let a = up(&f.h, "ticket", "PROJ-1", "one", 1);
        let b = up(&f.h, "ticket", "PROJ-2", "two", 1);
        f.h.save_vector(a, MODEL, &[1.0, 0.0], 1, None).unwrap();
        f.h.save_vector(b, MODEL, &[1.0, 0.0], 1, None).unwrap();
        Connection::open(&f.file).unwrap().execute("UPDATE item_vectors SET vec = ? WHERE item_id = ?", params![vec![1u8, 2, 3], a]).unwrap();
        assert_eq!(f.h.vector_for(a, MODEL).unwrap(), None);
        let hits = f.h.nearest(MODEL, &[1.0, 0.0], None, None, similar::MIN_COSINE).unwrap();
        assert_eq!(hits.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["jira:PROJ-2"]);
    }

    #[test]
    fn reopening_a_v2_database_keeps_its_vectors_a_newer_one_is_refused() {
        let f = open();
        let a = up(&f.h, "ticket", "PROJ-1", "one", 1);
        f.h.save_vector(a, MODEL, &[1.0, 0.0], 1, None).unwrap();
        drop(f.h);
        let again = HistoryDb::open(&f.file, Github).unwrap();
        assert_eq!(again.vector_stats().unwrap()["vectors"], 1);
        assert!(again.vector_for(a, MODEL).unwrap().is_some());
        drop(again);
        Connection::open(&f.file).unwrap().execute_batch(&format!("PRAGMA user_version = {}", schema::SCHEMA_VERSION + 1)).unwrap();
        assert!(HistoryDb::open(&f.file, Github).err().unwrap().contains("newer companion"));
    }

    #[test]
    fn placeholder_vectors_one_number_dont_count_as_real_vectors_and_never_match() {
        let f = open();
        let a = up(&f.h, "ticket", "PROJ-1", "one", 1);
        f.h.save_vector(a, MODEL, &[1.0], 1, None).unwrap();
        assert!(!f.h.has_vectors(MODEL).unwrap());
        assert!(f.h.nearest(MODEL, &[1.0, 0.0], None, None, similar::MIN_COSINE).unwrap().is_empty());
        let b = up(&f.h, "ticket", "PROJ-2", "two", 1);
        f.h.save_vector(b, MODEL, &[1.0, 0.0], 1, None).unwrap();
        assert!(f.h.has_vectors(MODEL).unwrap());
        assert!(!f.h.has_vectors("other").unwrap() && !f.h.has_vectors("").unwrap());
    }

    // ---- the recorder and pre-warm metrics over the real store ----

    const T0: i64 = 1_000_000;

    fn id(n: u32) -> String {
        format!("11111111-1111-4111-8111-{n:012}")
    }

    fn job(n: u32, over: Value) -> Value {
        let mut j = json!({ "id": id(n), "featureId": "resolve-conflict", "status": "awaiting-approval", "data": {}, "scopeKey": "bitbucket:CI/sample-app#12", "startedVia": "watcher", "watcher": "conflicts", "createdAt": T0, "updatedAt": T0 });
        for (k, v) in over.as_object().unwrap() {
            if v.is_null() {
                j.as_object_mut().unwrap().remove(k);
            } else {
                j[k] = v.clone();
            }
        }
        j
    }

    fn rules() -> HistoryRules {
        HistoryRules { read_only: vec!["analyze-issue".into()], milestone: vec![], event_only: vec![], git: Some(GitRemoteRules::BitbucketDc) }
    }

    #[test]
    fn a_watchers_job_carries_the_watcher_into_its_event_metrics_others_dont() {
        let ops = ops_for_job(&job(1, json!({ "status": "approved" })), T0 + 60_000, &rules()).unwrap();
        assert_eq!(ops.event["metrics"], json!({ "durationMs": 60_000, "watcher": "conflicts", "tier": "claude", "conflicted": false }));
        let plain = ops_for_job(&job(2, json!({ "status": "approved", "startedVia": "extension", "watcher": null })), T0 + 1, &rules()).unwrap();
        assert!(plain.event["metrics"].get("watcher").is_none());
        assert_eq!(crate::history_record::expired_event(&job(3, json!({})), T0 + 5, &rules()).unwrap()["metrics"]["watcher"], "conflicts");
    }

    #[test]
    fn prewarm_metrics_reads_the_recorded_events_back_out_of_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(Mutex::new(T0));
        let c = clock.clone();
        let db = Arc::new(HistoryDb::open_with_clock(&dir.path().join("history.db"), GitRemoteRules::BitbucketDc, move || *c.lock().unwrap()).unwrap());
        struct Shared(Arc<HistoryDb>);
        impl History for Shared {
            fn upsert_item(&self, item: &ItemOp) -> Result<i64, String> {
                History::upsert_item(&*self.0, item)
            }
            fn add_edge(&self, s: i64, d: i64, rel: &str, at: i64) -> Result<(), String> {
                History::add_edge(&*self.0, s, d, rel, at)
            }
            fn record_event(&self, ev: &Value) -> Result<(), String> {
                History::record_event(&*self.0, ev)
            }
        }
        let c2 = clock.clone();
        let recorder = Recorder::new(Box::new(Shared(db.clone())), rules(), Box::new(|_| {}), Box::new(move || *c2.lock().unwrap()));
        recorder.on_transition(&job(1, json!({ "status": "approved" })));
        recorder.on_transition(&job(2, json!({ "status": "rejected" })));
        let analysis = job(3, json!({ "featureId": "analyze-issue", "scopeKey": "jira:PROJ-7", "watcher": "assignedBugs" }));
        recorder.on_transition(&analysis);
        db.record_event_json(&used_event(&analysis, T0).unwrap()).unwrap();
        recorder.on_transition(&job(4, json!({ "status": "approved", "startedVia": "extension", "watcher": null })));
        assert_eq!(
            db.prewarm_metrics(30, None).unwrap(),
            [
                json!({ "watcher": "assignedBugs", "runs": 1, "used": 1, "discarded": 0, "failed": 0, "expired": 0, "usedFraction": 1 }),
                json!({ "watcher": "conflicts", "runs": 2, "used": 1, "discarded": 1, "failed": 0, "expired": 0, "usedFraction": 0.5 }),
            ]
        );
        *clock.lock().unwrap() += 31 * DAY;
        assert!(db.prewarm_metrics(30, None).unwrap().is_empty());
    }

    #[test]
    fn the_recorder_writes_a_ticket_to_pr_flow_through_the_real_store() {
        let f = open();
        let t2p = |status: &str, extra: Value| {
            let mut data = json!({ "issueKey": "PROJ-7", "summary": "Login fails", "repoKey": "ACME/sample-app", "branch": "bugfix/x", "base": "master", "ticketWorktree": { "dir": "/w/PROJ-7" }, "fixStartedAt": T0, "claudeSession": { "id": "44444444-4444-4444-8444-444444444444", "cwd": "/w/PROJ-7" } });
            for (k, v) in extra.as_object().unwrap() {
                data[k] = v.clone();
            }
            job(9, json!({ "featureId": "ticket-to-pr", "scopeKey": "jira:PROJ-7", "status": status, "startedVia": "extension", "watcher": null, "data": data }))
        };
        let rules = HistoryRules { milestone: vec!["ticket-to-pr".into()], ..rules() };
        let done = t2p("approved", json!({ "pr": { "id": 12, "url": "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12" } }));
        for j in [t2p("awaiting-approval", json!({})), done.clone(), done] {
            let ops = ops_for_job(&j, T0 + 90_000, &rules).unwrap();
            crate::history_record::apply_ops(&f.h, &ops).unwrap();
        }
        let ticket = f.h.get_item("PROJ-7").unwrap().unwrap();
        let rels: Vec<String> = ticket["edges"].as_array().unwrap().iter().map(|e| format!("{}:{}:{}", e["direction"].as_str().unwrap(), e["rel"].as_str().unwrap(), e["key"].as_str().unwrap())).collect();
        assert!(rels.contains(&"out:worktree:worktree:/w/PROJ-7".to_string()) && rels.contains(&"out:links:bitbucket:ACME/sample-app#12".to_string()), "{rels:?}");
        let wt = f.h.get_item("worktree:/w/PROJ-7").unwrap().unwrap();
        assert_eq!(wt["item"]["data"]["fixStartedAt"], T0);
        assert_eq!(wt["edges"].as_array().unwrap().iter().filter(|e| e["key"] == "bitbucket:ACME/sample-app#12").count(), 1, "repeats add no second edge");
        let events: i64 = Connection::open(&f.file).unwrap().query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(events, 2, "one event per (job, status)");
    }
}
