//! Pure pieces of the local history store (port of `core/history-schema.js`):
//! what may be stored, key normalization, excerpt limits, the SQL migrations
//! and the metrics roll-up. No I/O. Personal data only: never credentials,
//! cookies or raw secrets.

use crate::js_number::json_number;
use crate::js_text::{js_trim, locale_compare, slice_utf16, utf16_len};
use crate::prereqs::GitRemoteRules;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

pub const SCHEMA_VERSION: u32 = 2;
pub const KINDS: [&str; 7] = ["ticket", "pr", "analysis", "review_comment", "job", "session", "worktree"];
pub const RELS: [&str; 5] = ["analyzed", "fixes", "commented_on", "links", "worktree"];
pub const PROVENANCE: [&str; 7] = ["jenkins", "bitbucket", "github", "jira", "claude", "onprem-llm", "user"];
/// "used": a pre-warmed read-only result the user opened from the inbox.
pub const OUTCOMES: [&str; 6] = ["approved", "discarded", "expired", "failed", "completed", "used"];
pub const EXCERPT_MAX: usize = 2048;
pub const TITLE_MAX: usize = 300;
pub const KEY_MAX: usize = 300;
pub const DEFAULT_RETENTION_DAYS: i64 = 180;

pub const MIGRATIONS: [&str; 2] = [
    // v1
    r"
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
  ",
    // v2 (Phase 8): one embedding per item, for similar-item search. The
    // vector is an L2-normalised Float32 BLOB; deleting the item (forget,
    // retention) deletes it too.
    r"
  CREATE TABLE item_vectors (
    item_id INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL,
    item_updated_at INTEGER NOT NULL,
    at INTEGER NOT NULL
  );
  CREATE INDEX item_vectors_model ON item_vectors (model, dim);
  ",
];

fn is_control_but_text(c: char) -> bool {
    // [\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]: tab, newline and CR stay.
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}')
}

fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}')
}

/// Excerpt text: control characters out (not tab/newline), blank is `None`,
/// capped at 2 KB (UTF-16 units, as JS counts).
pub fn clip_excerpt(text: Option<&str>) -> Option<String> {
    let clean: String = text?.chars().filter(|c| !is_control_but_text(*c)).collect();
    if js_trim(&clean).is_empty() {
        return None;
    }
    Some(if utf16_len(&clean) > EXCERPT_MAX { slice_utf16(&clean, EXCERPT_MAX) } else { clean })
}

/// Title text: control characters become spaces, trimmed, capped at 300.
pub fn clip_title(text: Option<&str>) -> Option<String> {
    let spaced: String = text?.chars().map(|c| if is_control(c) { ' ' } else { c }).collect();
    let clean = js_trim(&spaced);
    if clean.is_empty() {
        return None;
    }
    Some(if utf16_len(clean) > TITLE_MAX { slice_utf16(clean, TITLE_MAX) } else { clean.to_string() })
}

static ISSUE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z][A-Za-z0-9]*)-([1-9][0-9]*)$").unwrap());
static PR_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)#([1-9][0-9]*)$").unwrap());

/// The stored form of a key a person typed: bare `PROJ-12` becomes
/// `jira:PROJ-12`, `ci/sample-app#7` the provider's PR key, anything else
/// already prefixed passes through. `None` if unusable.
pub fn normalize_key(input: Option<&str>, rules: GitRemoteRules) -> Option<String> {
    let s = js_trim(input?);
    if s.is_empty() || utf16_len(s) > KEY_MAX || s.chars().any(is_control) {
        return None;
    }
    if let Some(m) = ISSUE_KEY.captures(s) {
        return Some(format!("jira:{}-{}", m[1].to_uppercase(), &m[2]));
    }
    if let Some(m) = PR_REF.captures(s) {
        return Some(rules.pr_key(&m[1], &m[2], &m[3]));
    }
    Some(s.to_string())
}

fn median(sorted: &[f64]) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 { sorted[mid] } else { js_round((sorted[mid - 1] + sorted[mid]) / 2.0) })
}

/// `Math.round`: halves go toward +infinity.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

fn opt_num(n: Option<f64>) -> Value {
    n.map_or(Value::Null, json_number)
}

/// `events` rows (`feature_id`, `outcome`, `duration_ms`) become one summary
/// per feature+outcome, sorted, with median and average duration (null when no
/// row carried a duration).
pub fn summarize_events(rows: &[Value]) -> Vec<Value> {
    struct Group {
        feature_id: String,
        outcome: String,
        count: u64,
        durations: Vec<f64>,
    }
    let mut order: Vec<String> = vec![];
    let mut groups: HashMap<String, Group> = HashMap::new();
    for row in rows {
        let feature_id = crate::js_text::js_to_string(&row["feature_id"]);
        let outcome = crate::js_text::js_to_string(&row["outcome"]);
        let k = format!("{feature_id}\u{0}{outcome}");
        let g = groups.entry(k.clone()).or_insert_with(|| {
            order.push(k.clone());
            Group { feature_id, outcome, count: 0, durations: vec![] }
        });
        g.count += 1;
        if let Some(d) = row["duration_ms"].as_f64().filter(|d| d.is_finite()) {
            g.durations.push(d);
        }
    }
    let mut out: Vec<(String, String, Value)> = order
        .iter()
        .map(|k| {
            let g = &groups[k];
            let mut d = g.durations.clone();
            d.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg = (!d.is_empty()).then(|| js_round(d.iter().sum::<f64>() / d.len() as f64));
            let v = json!({
                "featureId": g.feature_id,
                "outcome": g.outcome,
                "count": g.count,
                "medianMs": opt_num(median(&d)),
                "avgMs": opt_num(avg),
            });
            (g.feature_id.clone(), g.outcome.clone(), v)
        })
        .collect();
    out.sort_by(|a, b| locale_compare(&a.0, &b.0).then_with(|| locale_compare(&a.1, &b.1)));
    out.into_iter().map(|x| x.2).collect()
}

/// Pre-warmed runs per watcher: how many a watcher started and how many the
/// user actually used (approved, or opened from the inbox). `rows` are events
/// rows whose `metrics_json` names a `watcher`; each job counts once.
pub fn summarize_prewarmed(rows: &[Value]) -> Vec<Value> {
    let mut job_order: Vec<String> = vec![];
    let mut jobs: HashMap<String, (String, BTreeSet<String>)> = HashMap::new();
    for row in rows {
        let metrics: Option<Value> = row["metrics_json"]
            .as_str()
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(s).ok());
        let Some(watcher) = metrics.as_ref().and_then(|m| m["watcher"].as_str()) else { continue };
        let job_id = crate::js_text::js_to_string(&row["job_id"]);
        let entry = jobs.entry(job_id.clone()).or_insert_with(|| {
            job_order.push(job_id.clone());
            (watcher.to_string(), BTreeSet::new())
        });
        entry.1.insert(crate::js_text::js_to_string(&row["outcome"]));
    }
    #[derive(Default)]
    struct G {
        runs: u64,
        used: u64,
        discarded: u64,
        failed: u64,
        expired: u64,
    }
    let mut watcher_order: Vec<String> = vec![];
    let mut groups: HashMap<String, G> = HashMap::new();
    for id in &job_order {
        let (watcher, outcomes) = &jobs[id];
        let g = groups.entry(watcher.clone()).or_insert_with(|| {
            watcher_order.push(watcher.clone());
            G::default()
        });
        g.runs += 1;
        let has = |o: &str| outcomes.contains(o);
        if has("approved") || has("used") {
            g.used += 1;
        } else if has("discarded") {
            g.discarded += 1;
        } else if has("failed") {
            g.failed += 1;
        } else if has("expired") {
            g.expired += 1;
        }
    }
    watcher_order.sort_by(|a, b| locale_compare(a, b));
    watcher_order
        .iter()
        .map(|w| {
            let g = &groups[w];
            let mut m = Map::new();
            m.insert("watcher".into(), json!(w));
            m.insert("runs".into(), json!(g.runs));
            m.insert("used".into(), json!(g.used));
            m.insert("discarded".into(), json!(g.discarded));
            m.insert("failed".into(), json!(g.failed));
            m.insert("expired".into(), json!(g.expired));
            let frac = (g.runs > 0).then(|| js_round(g.used as f64 / g.runs as f64 * 100.0) / 100.0);
            m.insert("usedFraction".into(), opt_num(frac));
            Value::Object(m)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use GitRemoteRules::{BitbucketDc, Github};

    #[test]
    fn normalize_key_maps_bare_issue_keys_and_pr_refs_to_the_scope_key_forms() {
        assert_eq!(normalize_key(Some("proj-12"), Github).as_deref(), Some("jira:PROJ-12"));
        assert_eq!(normalize_key(Some(" PROJ-12 "), Github).as_deref(), Some("jira:PROJ-12"));
        assert_eq!(normalize_key(Some("ci/Sample-App#7"), Github).as_deref(), Some("github:ci/sample-app#7"));
        assert_eq!(normalize_key(Some("ci/Sample-App#7"), BitbucketDc).as_deref(), Some("bitbucket:CI/sample-app#7"));
        assert_eq!(normalize_key(Some("jira:PROJ-12"), Github).as_deref(), Some("jira:PROJ-12"));
        assert_eq!(normalize_key(Some("job:abc"), Github).as_deref(), Some("job:abc"));
    }

    #[test]
    fn normalize_key_rejects_empty_control_character_and_oversized_keys() {
        assert_eq!(normalize_key(Some(""), Github), None);
        assert_eq!(normalize_key(Some("a\nb"), Github), None);
        assert_eq!(normalize_key(Some(&"x".repeat(301)), Github), None);
        assert_eq!(normalize_key(Some(&"x".repeat(300)), Github).map(|s| s.len()), Some(300));
        assert_eq!(normalize_key(None, Github), None);
        // Zero-leading numbers and non-ASCII digits are not issue numbers.
        assert_eq!(normalize_key(Some("proj-012"), Github).as_deref(), Some("proj-012"));
        assert_eq!(normalize_key(Some("proj-١٢"), Github).as_deref(), Some("proj-١٢"));
    }

    #[test]
    fn clip_excerpt_caps_at_2_kb_strips_control_characters_and_drops_blanks() {
        assert_eq!(clip_excerpt(Some(&"a".repeat(3000))).unwrap().len(), 2048);
        assert_eq!(clip_excerpt(Some("hi\u{0} there\u{7}")).as_deref(), Some("hi there"));
        assert_eq!(clip_excerpt(Some("keeps\nnewlines\tand tabs")).as_deref(), Some("keeps\nnewlines\tand tabs"));
        assert_eq!(clip_excerpt(Some("   ")), None);
        assert_eq!(clip_excerpt(None), None);
        // 2048 UTF-16 units: an astral char counts twice and is never cut in half.
        let s = format!("{}😀", "a".repeat(2047));
        assert_eq!(clip_excerpt(Some(&s)).unwrap(), "a".repeat(2047));
    }

    #[test]
    fn clip_title_turns_control_characters_into_spaces_and_trims() {
        assert_eq!(clip_title(Some("  a\nb\t c ")).as_deref(), Some("a b  c"));
        assert_eq!(clip_title(Some("\n\n")), None);
        assert_eq!(utf16_len(&clip_title(Some(&"t".repeat(400))).unwrap()), 300);
    }

    #[test]
    fn summarize_events_groups_by_feature_and_outcome_with_median_and_average() {
        let rows = vec![
            json!({ "feature_id": "resolve-conflict", "outcome": "approved", "duration_ms": 100 }),
            json!({ "feature_id": "resolve-conflict", "outcome": "approved", "duration_ms": 300 }),
            json!({ "feature_id": "resolve-conflict", "outcome": "approved", "duration_ms": 200 }),
            json!({ "feature_id": "resolve-conflict", "outcome": "discarded", "duration_ms": null }),
            json!({ "feature_id": "analyze-issue", "outcome": "completed", "duration_ms": 50 }),
        ];
        assert_eq!(
            Value::Array(summarize_events(&rows)),
            json!([
                { "featureId": "analyze-issue", "outcome": "completed", "count": 1, "medianMs": 50, "avgMs": 50 },
                { "featureId": "resolve-conflict", "outcome": "approved", "count": 3, "medianMs": 200, "avgMs": 200 },
                { "featureId": "resolve-conflict", "outcome": "discarded", "count": 1, "medianMs": null, "avgMs": null },
            ])
        );
    }

    #[test]
    fn summarize_events_rounds_the_even_median_and_the_average_half_up() {
        let rows: Vec<Value> = [1, 2].iter().map(|d| json!({ "feature_id": "f", "outcome": "o", "duration_ms": d })).collect();
        let out = summarize_events(&rows);
        assert_eq!(out[0]["medianMs"], json!(2)); // (1+2)/2 = 1.5 → 2
        assert_eq!(out[0]["avgMs"], json!(2));
    }

    fn pre(job: &str, outcome: &str, watcher: Option<&str>) -> Value {
        let metrics = watcher.map(|w| json!({ "watcher": w }).to_string());
        json!({ "job_id": job, "outcome": outcome, "metrics_json": metrics })
    }

    #[test]
    fn summarize_prewarmed_counts_each_job_once_and_ranks_used_before_the_rest() {
        let rows = vec![
            pre("j1", "completed", Some("assigned")),
            pre("j1", "used", Some("assigned")),
            pre("j2", "failed", Some("assigned")),
            pre("j3", "discarded", Some("assigned")),
            pre("j4", "expired", Some("assigned")),
            pre("j5", "approved", Some("review")),
            pre("j6", "completed", None),
            json!({ "job_id": "j7", "outcome": "used", "metrics_json": "{broken" }),
        ];
        assert_eq!(
            Value::Array(summarize_prewarmed(&rows)),
            json!([
                { "watcher": "assigned", "runs": 4, "used": 1, "discarded": 1, "failed": 1, "expired": 1, "usedFraction": 0.25 },
                { "watcher": "review", "runs": 1, "used": 1, "discarded": 0, "failed": 0, "expired": 0, "usedFraction": 1 },
            ])
        );
    }

    #[test]
    fn migrations_create_what_the_store_expects() {
        assert_eq!(MIGRATIONS.len() as u32, SCHEMA_VERSION);
        assert!(MIGRATIONS[0].contains("CREATE VIRTUAL TABLE items_fts"));
        assert!(MIGRATIONS[1].contains("item_vectors"));
    }
}
