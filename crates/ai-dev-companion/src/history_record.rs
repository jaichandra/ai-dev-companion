//! Turns a job's terminal transition into history rows (port of
//! `core/history-record.js`). Pure mapping (`ops_for_job`) plus a thin apply
//! step; the job layer's transition hook calls the recorder. Jobs are JSON, as
//! they are on disk and on the wire. Personal data only; text is clipped by
//! the store.

use crate::js_number::json_number;
use crate::js_text::{js_to_string, js_truthy};
use crate::packs::{HistoryFlags, PackRegistry};
use crate::prereqs::GitRemoteRules;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

/// Which features get which treatment, read from the loaded packs.
#[derive(Debug, Clone, Default)]
pub struct HistoryRules {
    /// Features whose job "finishes" at awaiting-approval (nothing to approve).
    pub read_only: Vec<String>,
    /// Features whose awaiting-approval is a milestone worth recording on its own.
    pub milestone: Vec<String>,
    /// Features that record only their timing event, no items.
    pub event_only: Vec<String>,
    pub git: Option<GitRemoteRules>,
}

impl HistoryRules {
    pub fn from_registry<F>(registry: &PackRegistry<F>, git: GitRemoteRules) -> Self {
        let ids = |flag: fn(&HistoryFlags) -> bool| registry.ids_with_history_flag(flag).into_iter().map(String::from).collect();
        Self {
            read_only: ids(|f| f.read_only),
            milestone: ids(|f| f.milestone),
            event_only: ids(|f| f.event_only),
            git: Some(git),
        }
    }

    fn git(&self) -> GitRemoteRules {
        self.git.unwrap_or_else(crate::prereqs::git_remote_rules)
    }
}

fn contains(list: &[String], id: &str) -> bool {
    list.iter().any(|x| x == id)
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub fn outcome_for(job: &Value, rules: &HistoryRules) -> Option<&'static str> {
    let feature = s(job, "featureId").unwrap_or("");
    match s(job, "status") {
        Some("approved") => Some("approved"),
        // Discarding a finished read-only analysis is not a job outcome; its "completed" event stands.
        Some("rejected") => (!contains(&rules.read_only, feature)).then_some("discarded"),
        Some("failed") => Some("failed"),
        Some("awaiting-approval") if contains(&rules.read_only, feature) || contains(&rules.milestone, feature) => Some("completed"),
        _ => None,
    }
}

static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\b(Bearer|Basic)\s+[^\s"'&]+"#).unwrap());
static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\b(token|password|secret|apikey|api_key)=[^\s&"']*"#).unwrap());
static URL_CREDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*)://[^\s/@]+@").unwrap());

/// Masks credentials that could ride along in an error message (argv, stderr).
pub fn redact_secrets(text: &str) -> String {
    let t = BEARER.replace_all(text, "$1 ***");
    let t = NAMED.replace_all(&t, "$1=***");
    URL_CREDS.replace_all(&t, "$1://***@").into_owned()
}

/// A JSON object from the entries that are present (JS drops `undefined`).
fn obj(entries: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = Map::new();
    for (k, v) in entries {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

fn finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite())
}

fn elapsed(at: i64, since: &Value) -> f64 {
    let n = crate::js_number::js_number(Some(since));
    (at as f64 - n).max(0.0)
}

fn metrics_for(job: &Value, at: i64) -> Value {
    let mut m = Map::new();
    m.insert("durationMs".into(), json_number(elapsed(at, &job["createdAt"])));
    let data = &job["data"];
    if s(job, "startedVia") == Some("watcher") {
        if let Some(w) = s(job, "watcher") {
            m.insert("watcher".into(), json!(w));
            m.insert("tier".into(), json!("claude"));
        }
    }
    if s(job, "featureId") == Some("resolve-conflict") {
        m.insert("conflicted".into(), json!(data["conflicted"] == json!(true)));
        if let Some(files) = job["result"]["files"].as_array() {
            m.insert("filesChanged".into(), json!(files.len()));
        }
    }
    if s(job, "featureId") == Some("ticket-to-pr") && s(job, "status") == Some("approved") {
        if let Some(started) = finite(&data["fixStartedAt"]) {
            // The headline number: from Start fix to the PR being open.
            m.insert("startFixToPrMs".into(), json_number((at as f64 - started).max(0.0)));
        }
    }
    Value::Object(m)
}

fn analysis_excerpt(analysis: &Value) -> Option<String> {
    if let Some(t) = analysis.as_str() {
        return Some(t.to_string());
    }
    if analysis.is_object() {
        // analyze-issue's report leads with "tldr"; the others are older shapes.
        for field in ["tldr", "summary", "rootCause", "title"] {
            if let Some(t) = analysis[field].as_str() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// One row the recorder will upsert; `ref_` names it for the edges.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOp {
    pub ref_: &'static str,
    pub kind: &'static str,
    pub key: String,
    pub repo: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub excerpt: Option<String>,
    pub data: Option<Value>,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeOp {
    pub from: &'static str,
    pub to: &'static str,
    pub rel: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ops {
    pub items: Vec<ItemOp>,
    pub edges: Vec<EdgeOp>,
    pub at: i64,
    pub event: Value,
}

static SUBJECT_PR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:bitbucket|github):([^/]+)/([^#]+)#([0-9]+)$").unwrap());

fn subject_for(job: &Value, data: &Value, at: i64) -> Option<ItemOp> {
    let scope_key = s(job, "scopeKey")?;
    let text = |v: &Value| v.as_str().map(String::from);
    let base = |kind, repo, title| ItemOp { ref_: "subject", kind, key: scope_key.to_string(), repo, title, url: None, excerpt: None, data: None, at };
    if scope_key.starts_with("jira:") {
        return Some(base("ticket", None, text(&data["summary"])));
    }
    let m = SUBJECT_PR.captures(scope_key)?;
    Some(base("pr", Some(format!("{}/{}", &m[1], &m[2])), text(&data["pr"]["title"])))
}

fn is_integer(v: &Value) -> Option<i64> {
    let n = v.as_f64()?;
    (n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15).then_some(n as i64)
}

/// Ticket to PR's worktree and PR, linked from the ticket.
fn ticket_to_pr_ops(data: &Value, items: &mut Vec<ItemOp>, edges: &mut Vec<EdgeOp>, at: i64, git: GitRemoteRules) {
    let wt = &data["ticketWorktree"];
    if let Some(dir) = wt["dir"].as_str().filter(|d| d.starts_with('/')) {
        items.push(ItemOp {
            ref_: "worktree",
            kind: "worktree",
            key: format!("worktree:{dir}"),
            repo: data["repoKey"].as_str().map(String::from),
            title: data["branch"].as_str().map(String::from),
            url: None,
            excerpt: None,
            data: Some(obj(vec![
                ("dir", Some(json!(dir))),
                ("repoKey", data.get("repoKey").cloned()),
                ("branch", data.get("branch").cloned()),
                ("base", data.get("base").cloned()),
                ("fixStartedAt", finite(&data["fixStartedAt"]).map(json_number)),
            ])),
            at,
        });
        edges.push(EdgeOp { from: "subject", to: "worktree", rel: "worktree" });
    }
    let pr = &data["pr"];
    let parsed = pr["url"].as_str().and_then(|u| git.parse_pr_url(u));
    if let (Some(parsed), Some(id)) = (parsed, is_integer(&pr["id"])) {
        items.push(ItemOp {
            ref_: "pr",
            kind: "pr",
            key: git.pr_key(&parsed.project, &parsed.repo, &id.to_string()),
            repo: Some(format!("{}/{}", parsed.project, parsed.repo)),
            title: pr["title"].as_str().map(String::from),
            url: pr["url"].as_str().map(String::from),
            excerpt: None,
            data: None,
            at,
        });
        edges.push(EdgeOp { from: "subject", to: "pr", rel: "links" });
        if items.iter().any(|i| i.ref_ == "worktree") {
            edges.push(EdgeOp { from: "worktree", to: "pr", rel: "links" });
        }
    }
}

fn event(job: &Value, status: &str, at: i64, duration: Value, outcome: &str, metrics: Value) -> Value {
    obj(vec![
        ("jobId", job.get("id").cloned()),
        ("featureId", job.get("featureId").cloned()),
        ("scopeKey", job.get("scopeKey").cloned()),
        ("status", Some(json!(status))),
        ("at", Some(json!(at))),
        ("durationMs", Some(duration)),
        ("outcome", Some(json!(outcome))),
        ("metrics", Some(metrics)),
    ])
}

pub fn ops_for_job(job: &Value, at: i64, rules: &HistoryRules) -> Option<Ops> {
    let outcome = outcome_for(job, rules)?;
    let empty = json!({});
    let data = if js_truthy(job.get("data")) { &job["data"] } else { &empty };
    let metrics = metrics_for(job, at);
    let ev = event(job, s(job, "status").unwrap_or(""), at, metrics["durationMs"].clone(), outcome, metrics);
    let feature = s(job, "featureId").unwrap_or("");
    let (mut items, mut edges) = (vec![], vec![]);
    if contains(&rules.event_only, feature) {
        return Some(Ops { items, edges, at, event: ev });
    }

    let result = &job["result"];
    let summary = js_truthy(result.get("summary")).then(|| &result["summary"]);
    let title = match summary {
        Some(v) => v.as_str().map(redact_secrets),
        None => Some(redact_secrets(&format!("{feature} {}", s(job, "status").unwrap_or("undefined")))),
    };
    let excerpt_src = if js_truthy(job.get("error")) { job.get("error") } else { summary };
    items.push(ItemOp {
        ref_: "job",
        kind: "job",
        key: format!("job:{}", js_to_string(&job["id"])),
        repo: None,
        title,
        url: None,
        excerpt: excerpt_src.and_then(Value::as_str).map(redact_secrets),
        data: Some(obj(vec![
            ("featureId", job.get("featureId").cloned()),
            ("status", job.get("status").cloned()),
            ("scopeKey", job.get("scopeKey").cloned()),
            ("startedVia", job.get("startedVia").cloned()),
        ])),
        at,
    });

    let subject = subject_for(job, data, at);
    if let Some(subject) = &subject {
        items.push(subject.clone());
        let rel = if feature != "analyze-issue" && s(job, "status") == Some("approved") { "fixes" } else { "links" };
        edges.push(EdgeOp { from: "job", to: "subject", rel });
        if feature == "analyze-issue" && js_truthy(data.get("analysis")) {
            items.push(ItemOp {
                ref_: "analysis",
                kind: "analysis",
                key: format!("analysis:{}", s(job, "scopeKey").unwrap_or("")),
                repo: data["repoKey"].as_str().map(String::from),
                title: Some(format!("Analysis of {}", subject.key.strip_prefix("jira:").unwrap_or(&subject.key))),
                url: None,
                excerpt: analysis_excerpt(&data["analysis"]),
                data: None,
                at,
            });
            edges.push(EdgeOp { from: "subject", to: "analysis", rel: "analyzed" });
        }
    }

    let session_id = data["claudeSession"]["id"].as_str();
    if let Some(id) = session_id {
        let session = &data["claudeSession"];
        items.push(ItemOp {
            ref_: "session",
            kind: "session",
            key: format!("session:{id}"),
            repo: None,
            title: Some(format!("{feature} session")),
            url: None,
            excerpt: None,
            data: Some(obj(vec![("cwd", session.get("cwd").cloned()), ("permissionMode", session.get("permissionMode").cloned())])),
            at,
        });
        edges.push(EdgeOp { from: "job", to: "session", rel: "links" });
    }

    if feature == "ticket-to-pr" && subject.is_some() {
        ticket_to_pr_ops(data, &mut items, &mut edges, at, rules.git());
        if session_id.is_some() && items.iter().any(|i| i.ref_ == "worktree") {
            edges.push(EdgeOp { from: "worktree", to: "session", rel: "links" });
        }
    }
    Some(Ops { items, edges, at, event: ev })
}

/// What the store offers the recorder.
pub trait History {
    fn upsert_item(&self, item: &ItemOp) -> Result<i64, String>;
    fn add_edge(&self, src: i64, dst: i64, rel: &str, at: i64) -> Result<(), String>;
    fn record_event(&self, event: &Value) -> Result<(), String>;
}

pub fn apply_ops(history: &dyn History, ops: &Ops) -> Result<(), String> {
    let mut ids: Vec<(&str, i64)> = vec![];
    for item in &ops.items {
        let id = history.upsert_item(item)?;
        ids.retain(|(r, _)| *r != item.ref_);
        ids.push((item.ref_, id));
    }
    let find = |r: &str| ids.iter().find(|(k, _)| *k == r).map(|(_, id)| *id);
    for edge in &ops.edges {
        if let (Some(src), Some(dst)) = (find(edge.from), find(edge.to)) {
            history.add_edge(src, dst, edge.rel, ops.at)?;
        }
    }
    history.record_event(&ops.event)
}

/// A job that aged out before anyone approved or discarded it; `None` when its status already has an event.
pub fn expired_event(job: &Value, at: i64, rules: &HistoryRules) -> Option<Value> {
    if outcome_for(job, rules).is_some() {
        return None;
    }
    let duration = json_number(elapsed(at, &job["createdAt"]));
    let mut metrics = Map::new();
    metrics.insert("durationMs".into(), duration.clone());
    metrics.insert("lastStatus".into(), job.get("status").cloned().unwrap_or(Value::Null));
    if s(job, "startedVia") == Some("watcher") {
        if let Some(w) = s(job, "watcher") {
            metrics.insert("watcher".into(), json!(w));
        }
    }
    Some(event(job, "expired", at, duration, "expired", Value::Object(metrics)))
}

/// The inbox's "opened" click on a pre-warmed job: the "used" outcome of a
/// read-only result (an approve is how a writing one gets used). `None` for a
/// job no watcher started.
pub fn used_event(job: &Value, at: i64) -> Option<Value> {
    if s(job, "startedVia") != Some("watcher") {
        return None;
    }
    let watcher = s(job, "watcher")?;
    Some(event(job, "opened", at, json_number(elapsed(at, &job["createdAt"])), "used", json!({ "watcher": watcher, "opened": true })))
}

pub struct Recorder {
    history: Box<dyn History + Send + Sync>,
    rules: HistoryRules,
    log: Box<dyn Fn(&str) + Send + Sync>,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    frozen: AtomicBool,
}

impl Recorder {
    pub fn new(
        history: Box<dyn History + Send + Sync>,
        rules: HistoryRules,
        log: Box<dyn Fn(&str) + Send + Sync>,
        now: Box<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self { history, rules, log, now, frozen: AtomicBool::new(false) }
    }

    /// Never fails: a store error is logged and dropped.
    pub fn on_transition(&self, job: &Value) {
        if self.frozen.load(Ordering::SeqCst) {
            return;
        }
        let result = match ops_for_job(job, (self.now)(), &self.rules) {
            Some(ops) => apply_ops(self.history.as_ref(), &ops),
            None => Ok(()),
        };
        if let Err(e) = result {
            let id = job.get("id").map_or_else(|| "undefined".to_string(), js_to_string);
            (self.log)(&format!("couldn't record job {id} in the history: {e}"));
        }
    }

    /// Stops recording: used at shutdown, when killed runs would otherwise log as failures.
    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const T0: i64 = 1_000_000;
    const WT: &str = "/Users/me/gitviews/sample-app.worktrees/PROJ-7";
    const SESSION_ID: &str = "44444444-4444-4444-8444-444444444444";
    const PR_URL: &str = "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12";

    fn rules() -> HistoryRules {
        HistoryRules {
            read_only: vec!["analyze-issue".into(), "ticket-workspace".into()],
            milestone: vec!["ticket-to-pr".into()],
            event_only: vec!["ticket-workspace".into()],
            git: Some(GitRemoteRules::BitbucketDc),
        }
    }

    fn job(over: Value) -> Value {
        let mut j = json!({
            "id": "11111111-1111-4111-8111-111111111111",
            "featureId": "analyze-issue",
            "status": "awaiting-approval",
            "data": {},
            "createdAt": T0,
            "updatedAt": T0 + 5000,
        });
        for (k, v) in over.as_object().unwrap() {
            j[k] = v.clone();
        }
        j
    }

    fn ticket_to_pr_data() -> Value {
        json!({
            "issueKey": "PROJ-7",
            "summary": "Login fails",
            "repoKey": "ACME/sample-app",
            "branch": "bugfix/PROJ-7-login-fails",
            "base": "master",
            "ticketWorktree": { "dir": WT },
            "fixStartedAt": T0,
            "claudeSession": { "id": SESSION_ID, "cwd": WT, "permissionMode": "plan" },
        })
    }

    fn t2p(over: Value) -> Value {
        let mut j = job(json!({ "featureId": "ticket-to-pr", "scopeKey": "jira:PROJ-7", "data": ticket_to_pr_data() }));
        for (k, v) in over.as_object().unwrap() {
            j[k] = v.clone();
        }
        j
    }

    fn with_data(extra: Value) -> Value {
        let mut d = ticket_to_pr_data();
        for (k, v) in extra.as_object().unwrap() {
            d[k] = v.clone();
        }
        d
    }

    fn ops(j: &Value, at: i64) -> Ops {
        ops_for_job(j, at, &rules()).unwrap()
    }

    fn kinds(o: &Ops) -> Vec<&'static str> {
        o.items.iter().map(|i| i.kind).collect()
    }

    #[test]
    fn outcome_for_maps_terminal_states_and_only_read_only_features_finish_at_awaiting_approval() {
        let r = rules();
        assert_eq!(outcome_for(&job(json!({ "status": "approved" })), &r), Some("approved"));
        assert_eq!(outcome_for(&job(json!({ "status": "rejected" })), &r), None);
        assert_eq!(outcome_for(&job(json!({ "featureId": "resolve-conflict", "status": "rejected" })), &r), Some("discarded"));
        assert_eq!(outcome_for(&job(json!({ "status": "failed" })), &r), Some("failed"));
        assert_eq!(outcome_for(&job(json!({})), &r), Some("completed"));
        assert_eq!(outcome_for(&job(json!({ "featureId": "resolve-conflict" })), &r), None);
        assert_eq!(outcome_for(&job(json!({ "status": "running" })), &r), None);
    }

    #[test]
    fn ops_for_job_returns_none_for_a_non_terminal_state() {
        assert!(ops_for_job(&job(json!({ "status": "running" })), T0 + 1, &rules()).is_none());
    }

    #[test]
    fn an_analysis_records_job_ticket_analysis_and_session_items_with_links() {
        let o = ops(
            &job(json!({
                "scopeKey": "jira:PROJ-7",
                "result": { "summary": "Null check missing", "files": [] },
                "data": {
                    "issueKey": "PROJ-7",
                    "summary": "Login fails",
                    "analysis": { "summary": "Missing null check in LoginForm" },
                    "claudeSession": { "id": "22222222-2222-4222-8222-222222222222", "cwd": "/w", "permissionMode": "plan" },
                },
            })),
            T0 + 5000,
        );
        let mut got: Vec<String> = o.items.iter().map(|i| format!("{}:{}", i.kind, i.key)).collect();
        got.sort();
        assert_eq!(
            got,
            [
                "analysis:analysis:jira:PROJ-7",
                "job:job:11111111-1111-4111-8111-111111111111",
                "session:session:22222222-2222-4222-8222-222222222222",
                "ticket:jira:PROJ-7",
            ]
        );
        let has = |from, to, rel| o.edges.contains(&EdgeOp { from, to, rel });
        assert!(has("subject", "analysis", "analyzed"));
        assert!(has("job", "subject", "links"));
        assert_eq!(o.items.iter().find(|i| i.kind == "analysis").unwrap().excerpt.as_deref(), Some("Missing null check in LoginForm"));
        assert_eq!(o.items.iter().find(|i| i.kind == "analysis").unwrap().title.as_deref(), Some("Analysis of PROJ-7"));
        assert_eq!(o.event["outcome"], "completed");
        assert_eq!(o.event["durationMs"], 5000);
        assert_eq!(o.event["scopeKey"], "jira:PROJ-7");
    }

    #[test]
    fn resolve_conflict_metrics_carry_file_count_and_whether_it_was_conflicted() {
        let o = ops(
            &job(json!({
                "featureId": "resolve-conflict",
                "status": "approved",
                "scopeKey": "bitbucket:CI/sample-app#3",
                "data": { "conflicted": true },
                "result": { "summary": "s", "files": [{ "path": "a" }, { "path": "b" }] },
            })),
            T0 + 90000,
        );
        assert_eq!(o.event["metrics"], json!({ "durationMs": 90000, "conflicted": true, "filesChanged": 2 }));
        assert!(o.edges.contains(&EdgeOp { from: "job", to: "subject", rel: "fixes" }));
        let pr = o.items.iter().find(|i| i.kind == "pr").unwrap();
        assert_eq!(pr.repo.as_deref(), Some("CI/sample-app"));
    }

    #[test]
    fn watcher_runs_carry_the_watcher_and_tier_in_their_metrics_in_js_key_order() {
        let o = ops(&job(json!({ "startedVia": "watcher", "watcher": "assignedBugs" })), T0 + 10);
        assert_eq!(serde_json::to_string(&o.event["metrics"]).unwrap(), r#"{"durationMs":10,"watcher":"assignedBugs","tier":"claude"}"#);
        assert_eq!(
            serde_json::to_string(&o.event).unwrap(),
            r#"{"jobId":"11111111-1111-4111-8111-111111111111","featureId":"analyze-issue","status":"awaiting-approval","at":1000010,"durationMs":10,"outcome":"completed","metrics":{"durationMs":10,"watcher":"assignedBugs","tier":"claude"}}"#
        );
    }

    #[test]
    fn expired_event_describes_a_pruned_job() {
        let j = job(json!({ "featureId": "resolve-conflict", "scopeKey": "jira:PROJ-7" }));
        assert_eq!(
            expired_event(&j, T0 + 9000, &rules()).unwrap(),
            json!({
                "jobId": "11111111-1111-4111-8111-111111111111",
                "featureId": "resolve-conflict",
                "scopeKey": "jira:PROJ-7",
                "status": "expired",
                "at": T0 + 9000,
                "durationMs": 9000,
                "outcome": "expired",
                "metrics": { "durationMs": 9000, "lastStatus": "awaiting-approval" },
            })
        );
        let w = job(json!({ "featureId": "resolve-conflict", "startedVia": "watcher", "watcher": "w" }));
        assert_eq!(expired_event(&w, T0 + 9000, &rules()).unwrap()["metrics"]["watcher"], "w");
    }

    #[test]
    fn expired_event_is_none_when_the_jobs_status_already_has_an_event() {
        assert!(expired_event(&job(json!({ "status": "failed" })), T0 + 9000, &rules()).is_none());
        assert!(expired_event(&job(json!({})), T0 + 9000, &rules()).is_none());
    }

    #[test]
    fn used_event_is_only_for_jobs_a_watcher_started() {
        let j = job(json!({ "startedVia": "watcher", "watcher": "assignedBugs", "scopeKey": "jira:PROJ-7" }));
        assert_eq!(
            used_event(&j, T0 + 100).unwrap(),
            json!({
                "jobId": "11111111-1111-4111-8111-111111111111",
                "featureId": "analyze-issue",
                "scopeKey": "jira:PROJ-7",
                "status": "opened",
                "at": T0 + 100,
                "durationMs": 100,
                "outcome": "used",
                "metrics": { "watcher": "assignedBugs", "opened": true },
            })
        );
        assert!(used_event(&job(json!({})), T0).is_none());
        assert!(used_event(&job(json!({ "startedVia": "watcher" })), T0).is_none());
        assert!(used_event(&Value::Null, T0).is_none());
    }

    #[test]
    fn redact_secrets_masks_tokens_passwords_and_url_credentials_and_leaves_ordinary_text() {
        assert_eq!(redact_secrets("Authorization: Bearer abc123"), "Authorization: Bearer ***");
        assert_eq!(redact_secrets("Authorization: Basic dXNlcjpwdw=="), "Authorization: Basic ***");
        assert_eq!(redact_secrets("x?Token=abc&y=1"), "x?Token=***&y=1");
        assert_eq!(redact_secrets(r#"password=hunter2 "secret=s3""#), r#"password=*** "secret=***""#);
        assert_eq!(redact_secrets("apikey=k1 api_key=k2"), "apikey=*** api_key=***");
        assert_eq!(redact_secrets("clone https://me:pw@host/x"), "clone https://***@host/x");
        assert_eq!(redact_secrets("Null check missing in LoginForm"), "Null check missing in LoginForm");
    }

    #[test]
    fn a_failed_jobs_stored_excerpt_carries_no_credentials() {
        let o = ops(&job(json!({ "status": "failed", "error": "git clone https://me:hunter2@host/x failed (Authorization: Bearer abc123)" })), T0 + 1);
        let excerpt = o.items.iter().find(|i| i.ref_ == "job").unwrap().excerpt.clone().unwrap();
        assert!(!excerpt.contains("hunter2") && !excerpt.contains("abc123"), "{excerpt}");
    }

    #[test]
    fn the_job_item_falls_back_to_feature_and_status_for_its_title() {
        let o = ops(&job(json!({ "status": "failed" })), T0 + 1);
        let item = &o.items[0];
        assert_eq!(item.title.as_deref(), Some("analyze-issue failed"));
        assert_eq!(item.excerpt, None);
        assert_eq!(item.data.as_ref().unwrap(), &json!({ "featureId": "analyze-issue", "status": "failed" }));
    }

    #[test]
    fn start_fix_is_recorded_at_awaiting_approval_the_ticket_links_to_its_worktree_and_the_worktree_to_the_session() {
        assert_eq!(outcome_for(&t2p(json!({})), &rules()), Some("completed"));
        let o = ops(&t2p(json!({})), T0 + 1000);
        let wt = o.items.iter().find(|i| i.kind == "worktree").unwrap();
        assert_eq!(wt.key, format!("worktree:{WT}"));
        assert_eq!(wt.repo.as_deref(), Some("ACME/sample-app"));
        assert_eq!(wt.title.as_deref(), Some("bugfix/PROJ-7-login-fails"));
        assert_eq!(
            wt.data.as_ref().unwrap(),
            &json!({ "dir": WT, "repoKey": "ACME/sample-app", "branch": "bugfix/PROJ-7-login-fails", "base": "master", "fixStartedAt": T0 })
        );
        assert!(o.edges.contains(&EdgeOp { from: "subject", to: "worktree", rel: "worktree" }));
        assert!(o.edges.contains(&EdgeOp { from: "worktree", to: "session", rel: "links" }));
        assert!(o.event["metrics"].get("startFixToPrMs").is_none());
    }

    #[test]
    fn create_pr_records_the_pr_from_the_ticket_and_the_worktree_and_the_start_fix_to_pr_time() {
        let approved = t2p(json!({ "status": "approved", "data": with_data(json!({ "pr": { "id": 12, "title": "PROJ-7: fix login", "url": PR_URL } })) }));
        let o = ops(&approved, T0 + 90_000);
        let pr = o.items.iter().find(|i| i.kind == "pr").unwrap();
        assert_eq!(pr.key, "bitbucket:ACME/sample-app#12");
        assert_eq!(pr.url.as_deref(), Some(PR_URL));
        assert_eq!(pr.repo.as_deref(), Some("ACME/sample-app"));
        assert!(o.edges.contains(&EdgeOp { from: "subject", to: "pr", rel: "links" }));
        assert!(o.edges.contains(&EdgeOp { from: "worktree", to: "pr", rel: "links" }));
        assert!(o.edges.contains(&EdgeOp { from: "job", to: "subject", rel: "fixes" }));
        assert_eq!(o.event["outcome"], "approved");
        assert_eq!(o.event["metrics"]["startFixToPrMs"], 90_000);
    }

    #[test]
    fn a_workspace_scan_records_only_its_timing_event() {
        let o = ops(&job(json!({ "featureId": "ticket-workspace", "scopeKey": "jira:PROJ-7" })), T0 + 700);
        assert!(o.items.is_empty() && o.edges.is_empty());
        assert_eq!(o.event["outcome"], "completed");
        assert_eq!(o.event["featureId"], "ticket-workspace");
    }

    #[test]
    fn bad_ticket_to_pr_data_records_nothing_it_shouldnt() {
        let at = T0 + 5000;
        let with_wt = |wt: Value| {
            let mut d = ticket_to_pr_data();
            match wt {
                Value::Null => {
                    d.as_object_mut().unwrap().remove("ticketWorktree");
                }
                v => d["ticketWorktree"] = v,
            }
            ops(&t2p(json!({ "data": d })), at)
        };
        assert!(!kinds(&with_wt(json!({ "dir": "relative/dir" }))).contains(&"worktree"));
        assert!(!kinds(&with_wt(Value::Null)).contains(&"worktree"));
        assert!(!kinds(&with_wt(json!({}))).contains(&"worktree"));
        // Non-finite or non-numeric start times.
        let nan = ops(&t2p(json!({ "status": "approved", "data": with_data(json!({ "fixStartedAt": null })) })), at);
        assert!(nan.event["metrics"].get("startFixToPrMs").is_none());
        assert!(nan.items.iter().find(|i| i.kind == "worktree").unwrap().data.as_ref().unwrap().get("fixStartedAt").is_none());
        let text = ops(&t2p(json!({ "status": "approved", "data": with_data(json!({ "fixStartedAt": "5" })) })), at);
        assert!(text.event["metrics"].get("startFixToPrMs").is_none());
        for url in [json!("not a url"), json!("https://bb.example/projects/ACME/repos/sample-app"), json!("javascript:alert(1)"), json!(5), Value::Null] {
            let o = ops(&t2p(json!({ "status": "approved", "data": with_data(json!({ "pr": { "id": 12, "url": url } })) })), at);
            assert!(!kinds(&o).contains(&"pr"), "{url}");
        }
        let string_id = ops(&t2p(json!({ "status": "approved", "data": with_data(json!({ "pr": { "id": "12", "url": PR_URL } })) })), at);
        assert!(!kinds(&string_id).contains(&"pr"));
        let fractional = ops(&t2p(json!({ "status": "approved", "data": with_data(json!({ "pr": { "id": 12.5, "url": PR_URL } })) })), at);
        assert!(!kinds(&fractional).contains(&"pr"));
        let failed = ops(&t2p(json!({ "status": "failed", "error": "boom", "data": with_data(json!({ "pr": { "id": 12, "url": PR_URL } })) })), at);
        assert_eq!(failed.event["outcome"], "failed");
        assert!(failed.event["metrics"].get("startFixToPrMs").is_none());
    }

    // ---- the recorder over a fake store ----

    #[derive(Default)]
    struct Store {
        items: Mutex<Vec<(String, String, Option<String>)>>,
        edges: Mutex<Vec<(i64, i64, String)>>,
        events: Mutex<Vec<Value>>,
        fail: bool,
    }

    struct Shared(Arc<Store>);

    impl History for Shared {
        fn upsert_item(&self, item: &ItemOp) -> Result<i64, String> {
            if self.0.fail {
                return Err("disk full".into());
            }
            let mut items = self.0.items.lock().unwrap();
            if let Some(i) = items.iter().position(|(k, key, _)| *k == item.kind && *key == item.key) {
                return Ok(i as i64 + 1);
            }
            items.push((item.kind.to_string(), item.key.clone(), item.title.clone()));
            Ok(items.len() as i64)
        }
        fn add_edge(&self, src: i64, dst: i64, rel: &str, _at: i64) -> Result<(), String> {
            let mut e = self.0.edges.lock().unwrap();
            if !e.iter().any(|x| x.0 == src && x.1 == dst && x.2 == rel) {
                e.push((src, dst, rel.to_string()));
            }
            Ok(())
        }
        fn record_event(&self, event: &Value) -> Result<(), String> {
            self.0.events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    fn recorder(store: &Arc<Store>, logs: &Arc<Mutex<Vec<String>>>) -> Recorder {
        let logs = logs.clone();
        Recorder::new(Box::new(Shared(store.clone())), rules(), Box::new(move |m| logs.lock().unwrap().push(m.to_string())), Box::new(|| T0 + 5000))
    }

    #[test]
    fn apply_ops_writes_the_rows_and_a_recorder_never_throws_or_writes_once_frozen() {
        let store = Arc::new(Store::default());
        let logs = Arc::new(Mutex::new(vec![]));
        let r = recorder(&store, &logs);
        r.on_transition(&job(json!({ "scopeKey": "jira:PROJ-7", "data": { "summary": "Login fails", "analysis": "It is a null check." } })));
        assert!(store.items.lock().unwrap().iter().any(|(k, key, title)| k == "ticket" && key == "jira:PROJ-7" && title.as_deref() == Some("Login fails")));
        assert!(store.items.lock().unwrap().iter().any(|(k, key, _)| k == "analysis" && key == "analysis:jira:PROJ-7"));
        assert_eq!(store.edges.lock().unwrap().len(), 2); // job -> ticket, ticket -> analysis
        assert_eq!(store.events.lock().unwrap()[0]["outcome"], "completed");
        r.freeze();
        r.on_transition(&job(json!({ "id": "33333333-3333-4333-8333-333333333333", "status": "failed" })));
        assert_eq!(store.events.lock().unwrap().len(), 1);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failing_store_is_logged_not_thrown() {
        let store = Arc::new(Store { fail: true, ..Default::default() });
        let logs = Arc::new(Mutex::new(vec![]));
        recorder(&store, &logs).on_transition(&job(json!({})));
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("disk full") && logs[0].contains("11111111-1111-4111-8111-111111111111"), "{logs:?}");
    }

    #[test]
    fn start_fix_then_create_pr_links_worktree_to_pr_once_and_repeats_add_nothing_new() {
        let store = Arc::new(Store::default());
        let logs = Arc::new(Mutex::new(vec![]));
        let r = recorder(&store, &logs);
        r.on_transition(&t2p(json!({})));
        let approved = t2p(json!({ "status": "approved", "data": with_data(json!({ "pr": { "id": 12, "url": PR_URL } })) }));
        r.on_transition(&approved);
        r.on_transition(&approved);
        let items = store.items.lock().unwrap();
        assert_eq!(items.iter().filter(|(k, ..)| k == "pr").count(), 1);
        let idx = |kind: &str| items.iter().position(|(k, ..)| k == kind).unwrap() as i64 + 1;
        let edges = store.edges.lock().unwrap();
        assert_eq!(edges.iter().filter(|e| e.0 == idx("worktree") && e.1 == idx("pr") && e.2 == "links").count(), 1);
        assert!(edges.iter().any(|e| e.0 == idx("worktree") && e.1 == idx("session")));
        assert!(logs.lock().unwrap().is_empty());
    }
}
