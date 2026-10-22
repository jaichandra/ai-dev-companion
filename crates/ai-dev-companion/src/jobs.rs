//! The job store and the `Feature` contract (port of `core/jobs.ts`).
//!
//! Jobs are JSON values (`Value`) with the shape the extension and the on-disk
//! files already use (`id, featureId, status, data, progress?, result?, error?,
//! createdAt, updatedAt, scopeKey?, startedVia?, watcher?`), so persistence,
//! history recording and the HTTP layer pass them through unchanged. The store
//! hands out *copies*: features change a job only through `update` and
//! `patch_data`, which keeps every write going through persistence and the
//! transition listeners.
//!
//! Persistence is opt-in write-through (off by default, and in every test); see
//! `job_files` for the format, restart reconciliation and pruning.

use crate::atlassian::AuthContext;
use crate::history_db::HistoryDb;
use crate::ids::uuid_v4;
use crate::job_files::{self, LoadOptions, PersistAction};
use crate::providers::Providers;
use crate::scope_key::lookup_job;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex, MutexGuard};

pub const JOB_STATUSES: [&str; 8] = ["running", "awaiting-approval", "approving", "approved", "rejecting", "rejected", "failed", "pending-start"];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> i64 {
    job_files::now_ms() as i64
}

type TransitionListener = Box<dyn Fn(&Value, &str) + Send + Sync>;

struct Persistence {
    dir: String,
    feature_ids: Vec<String>,
}

#[derive(Default)]
struct Inner {
    /// Insertion-ordered, so ties in `created_at` list the way they were made (as a JS `Map` does).
    jobs: Map<String, Value>,
    persistence: Option<Persistence>,
}

#[derive(Default)]
pub struct JobStore {
    inner: Mutex<Inner>,
    listeners: Mutex<Vec<TransitionListener>>,
}

/// What `enable_persistence` found on disk.
#[derive(Debug, Default, PartialEq)]
pub struct PersistenceLoad {
    pub loaded: usize,
    pub reconciled: Vec<String>,
    pub pruned: Vec<String>,
    pub pruned_jobs: Vec<Value>,
    pub dropped: Vec<String>,
}

pub struct PersistenceOptions<'a> {
    pub dir: &'a str,
    pub worktree_root: &'a str,
    pub feature_ids: Vec<String>,
    pub session_cwd_ok: &'a dyn Fn(&str) -> bool,
    pub adopted_worktree_ok: &'a dyn Fn(&str, &str) -> bool,
}

impl JobStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Calls `listener(job, from_status)` after a job's status changes (from `update`). A panicking
    /// listener is ignored: history must never break a job.
    pub fn on_transition(&self, listener: impl Fn(&Value, &str) + Send + Sync + 'static) {
        lock(&self.listeners).push(Box::new(listener));
    }

    fn notify_transition(&self, job: &Value, from: &str) {
        for listener in lock(&self.listeners).iter() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(job, from))).is_err() {
                eprintln!("[jobs] WARN - transition listener failed");
            }
        }
    }

    fn insert(&self, job: Value) -> Value {
        let mut inner = lock(&self.inner);
        let id = job["id"].as_str().unwrap_or_default().to_string();
        inner.jobs.insert(id, job.clone());
        Self::persist(&inner, &job);
        job
    }

    pub fn create(&self, feature_id: &str, data: Map<String, Value>) -> Value {
        let t = now();
        self.insert(json!({ "id": uuid_v4(), "featureId": feature_id, "status": "running", "data": data, "createdAt": t, "updatedAt": t }))
    }

    /// Creates a job that's waiting for a browser Start click before it actually runs: how an MCP
    /// tool with write access hands off to the extension instead of driving Claude with Bash
    /// access on its own. `data` is `{ payload }`, what `Feature::start` would have received.
    pub fn create_pending(&self, feature_id: &str, payload: Value, scope_key: Option<&str>) -> Value {
        let t = now();
        let mut job = json!({ "id": uuid_v4(), "featureId": feature_id, "status": "pending-start", "data": { "payload": payload }, "startedVia": "mcp", "createdAt": t, "updatedAt": t });
        if let Some(k) = scope_key {
            job["scopeKey"] = json!(k);
        }
        self.insert(job)
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        lock(&self.inner).jobs.get(id).cloned()
    }

    /// Every job, newest first.
    pub fn list(&self) -> Vec<Value> {
        let mut jobs: Vec<Value> = lock(&self.inner).jobs.values().cloned().collect();
        jobs.sort_by(|a, b| b["createdAt"].as_f64().partial_cmp(&a["createdAt"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        jobs
    }

    /// The newest still-relevant job an MCP tool started for `scope_key` on `feature_id`.
    pub fn lookup(&self, scope_key: &str, feature_id: &str) -> Option<Value> {
        let jobs: Vec<Value> = lock(&self.inner).jobs.values().cloned().collect();
        lookup_job(&jobs, scope_key, feature_id).cloned()
    }

    /// Puts back a job a feature kept on disk across a restart; an existing job wins.
    pub fn restore(&self, job: Value) -> Value {
        let mut inner = lock(&self.inner);
        let id = job["id"].as_str().unwrap_or_default().to_string();
        if let Some(existing) = inner.jobs.get(&id) {
            return existing.clone();
        }
        inner.jobs.insert(id, job.clone());
        Self::persist(&inner, &job);
        job
    }

    /// Shallow-merges `patch` into the job and stamps `updatedAt`. A `null` value in `patch` removes
    /// that field (what assigning `undefined` does to the job's JSON in Node).
    pub fn update(&self, id: &str, patch: Map<String, Value>) -> Result<Value, String> {
        let (job, from) = {
            let mut inner = lock(&self.inner);
            let job = inner.jobs.get_mut(id).ok_or_else(|| format!("Unknown job id: {id}"))?;
            let from = job["status"].as_str().unwrap_or_default().to_string();
            let obj = job.as_object_mut().expect("a job is an object");
            for (k, v) in patch {
                if v.is_null() {
                    obj.shift_remove(&k);
                } else {
                    obj.insert(k, v);
                }
            }
            obj.insert("updatedAt".into(), json!(now()));
            let job = job.clone();
            Self::persist(&inner, &job);
            (job, from)
        };
        if job["status"].as_str() != Some(from.as_str()) {
            self.notify_transition(&job, &from);
        }
        Ok(job)
    }

    /// `update` with one field set.
    pub fn set(&self, id: &str, key: &str, value: Value) -> Result<Value, String> {
        self.update(id, Map::from_iter([(key.to_string(), value)]))
    }

    /// Sets the progress label only while the job is still running: a throttled label can arrive
    /// just after a cancel or a failure, and must not overwrite that state.
    pub fn set_progress_if_running(&self, id: &str, step_id: &str, label: &str) {
        let running = lock(&self.inner).jobs.get(id).is_some_and(|j| j["status"] == "running");
        if running {
            let _ = self.set(id, "progress", json!({ "stepId": step_id, "label": label }));
        }
    }

    /// Shallow-merges `partial` into `job.data` (unlike `update`, which replaces the whole value).
    pub fn patch_data(&self, id: &str, partial: Map<String, Value>) -> Result<Value, String> {
        let mut inner = lock(&self.inner);
        let job = inner.jobs.get_mut(id).ok_or_else(|| format!("Unknown job id: {id}"))?;
        let obj = job.as_object_mut().expect("a job is an object");
        let mut data = obj.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
        data.extend(partial);
        obj.insert("data".into(), Value::Object(data));
        obj.insert("updatedAt".into(), json!(now()));
        let job = job.clone();
        Self::persist(&inner, &job);
        Ok(job)
    }

    /// `patch_data` with one field set.
    pub fn set_data(&self, id: &str, key: &str, value: Value) -> Result<Value, String> {
        self.patch_data(id, Map::from_iter([(key.to_string(), value)]))
    }

    /// How many jobs are mid-flight: work a service restart would cut off.
    pub fn active_count(&self) -> usize {
        lock(&self.inner).jobs.values().filter(|j| matches!(j["status"].as_str(), Some("running" | "approving" | "rejecting"))).count()
    }

    /// Turns on write-through persistence and loads whatever is already on disk (only for
    /// `feature_ids`). Loaded jobs go through `restore`, so a job already in memory wins.
    pub fn enable_persistence(&self, opts: PersistenceOptions<'_>) -> PersistenceLoad {
        let load_opts = LoadOptions {
            worktree_root: Some(opts.worktree_root),
            session_cwd_ok: opts.session_cwd_ok,
            adopted_worktree_ok: opts.adopted_worktree_ok,
            log: &|m| eprintln!("[jobs] WARN - {m}"),
            ..Default::default()
        };
        let loaded = job_files::load_jobs(opts.dir, &load_opts);
        let count = loaded.jobs.len();
        for job in loaded.jobs {
            self.restore(job);
        }
        // Write-through starts from here on: a reconciled job's on-disk copy is already correct.
        lock(&self.inner).persistence = Some(Persistence { dir: opts.dir.to_string(), feature_ids: opts.feature_ids });
        PersistenceLoad { loaded: count, reconciled: loaded.reconciled, pruned: loaded.pruned, pruned_jobs: loaded.pruned_jobs, dropped: loaded.dropped }
    }

    /// Stops write-through for the rest of this process's life (the in-memory store keeps working).
    /// The shutdown handler calls this before killing the detached claude children, so a dying run
    /// can't save its job as "failed" over the restart message a reconciled job would carry.
    pub fn freeze_persistence(&self) {
        lock(&self.inner).persistence = None;
    }

    /// Drops persisted jobs nobody acted on for `max_age_ms`, from memory and disk, and returns them
    /// so the caller can remove their worktrees.
    pub fn sweep_stale(&self, max_age_ms: f64, now_ms: f64) -> Vec<Value> {
        let mut inner = lock(&self.inner);
        let Some(p) = inner.persistence.as_ref() else { return vec![] };
        let (dir, ids) = (p.dir.clone(), p.feature_ids.clone());
        let stale: Vec<String> = inner
            .jobs
            .values()
            .filter(|j| j["featureId"].as_str().is_some_and(|f| ids.iter().any(|i| i == f)) && job_files::is_stale(j, now_ms, max_age_ms))
            .filter_map(|j| j["id"].as_str().map(str::to_string))
            .collect();
        let mut swept = vec![];
        for id in stale {
            let Some(job) = inner.jobs.shift_remove(&id) else { continue };
            if let Err(e) = job_files::remove_job(&dir, &id) {
                eprintln!("[jobs] WARN - couldn't remove the file of stale job {id}: {e}");
            }
            swept.push(job);
        }
        swept
    }

    /// Write-through for one job. Never fails: a save failure only costs this job's restart
    /// resilience, not the job itself.
    fn persist(inner: &Inner, job: &Value) {
        let Some(p) = &inner.persistence else { return };
        let ids: Vec<&str> = p.feature_ids.iter().map(String::as_str).collect();
        let id = job["id"].as_str().unwrap_or_default();
        let result = match job_files::persist_action(job, &ids) {
            PersistAction::Skip => return,
            PersistAction::Remove => job_files::remove_job(&p.dir, id),
            PersistAction::Save => job_files::save_job(&p.dir, job).map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            eprintln!("[jobs] WARN - couldn't save job {id}: {e}");
        }
    }
}

// ---- The Feature contract ----

/// What a feature's methods fail with; the HTTP layer maps each to a status.
#[derive(Debug, Clone, PartialEq)]
pub enum FeatureError {
    /// A structured 409 (`code` plus `details`) the extension can act on, instead of a failed job.
    StartRefused { message: String, code: String, details: Map<String, Value> },
    /// The job isn't in a status this operation supports right now: 409.
    JobStatus(String),
    /// The job's worktree (or other on-disk resource) is already gone: 410.
    WorktreeGone(String),
    /// Anything else: a failed job or a 500.
    Other(String),
}

impl FeatureError {
    pub fn refused(message: &str, code: &str, details: Map<String, Value>) -> Self {
        Self::StartRefused { message: message.into(), code: code.into(), details }
    }
}

impl std::fmt::Display for FeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StartRefused { message: m, .. } | Self::JobStatus(m) | Self::WorktreeGone(m) | Self::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for FeatureError {}

impl From<String> for FeatureError {
    fn from(m: String) -> Self {
        Self::Other(m)
    }
}

impl From<crate::atlassian::AtlassianError> for FeatureError {
    fn from(e: crate::atlassian::AtlassianError) -> Self {
        Self::Other(e.to_string())
    }
}

pub type FeatureResult<T> = Result<T, FeatureError>;

/// Per-request context passed alongside a feature's start/approve/reject.
#[derive(Debug, Clone, Default)]
pub struct FeatureContext {
    /// The page's browser session, relayed from the extension: how a feature acts as the logged-in
    /// user without any credential being configured.
    pub auth: AuthContext,
    /// The request body: approve/reject don't otherwise receive one, but a feature like
    /// create-jira-subtasks needs the panel's in-review edits on approve.
    pub body: Option<Value>,
    /// Set only by the service's own watchers, never from a request: the run has no click behind
    /// it, so a feature that runs Claude uses the stricter read-only background policy.
    pub background: bool,
}

/// Live status of whatever a finished job handed off outside this service (review-in-editor's
/// terminal review): `state` is "starting" | "running" | "finished" | "cancelled" | "ended".
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub state: String,
    pub exit_code: Option<i64>,
}

/// Contract every plugged-in feature implements. The core never inspects a feature's internals
/// beyond this shape. Optional operations have a `supports_*`/`action_names` probe so the HTTP layer
/// only registers the routes a feature really has.
#[async_trait]
pub trait Feature: Send + Sync {
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    /// Kicks off a job from an extension-supplied payload: creates the job via `JobStore::create`
    /// and drives it to "awaiting-approval" or "failed" (async work may continue after return).
    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value>;
    /// Applies the job's proposed result; must transition the job to "approved" or "failed".
    async fn approve(&self, job: Value, ctx: FeatureContext) -> FeatureResult<()>;
    /// Discards the proposed result and cleans up what the job created; "rejected" or "failed".
    async fn reject(&self, job: Value, ctx: FeatureContext) -> FeatureResult<()>;

    /// Stops a still-"running" job, undoes what it changed, and resolves once the job is "rejected".
    fn supports_cancel(&self) -> bool {
        false
    }
    async fn cancel(&self, _job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::Other("This feature can't be cancelled.".into()))
    }
    /// Posts a Jira comment for the job (analyze-issue): Markdown on `ctx.body.body`.
    fn supports_post_comment(&self) -> bool {
        false
    }
    async fn post_comment(&self, _job: Value, _ctx: FeatureContext) -> FeatureResult<Option<String>> {
        Err(FeatureError::Other("This feature doesn't post comments.".into()))
    }
    /// Live status of a hand-off; `None` when the job has nothing to track.
    async fn session_status(&self, _job: &Value) -> Option<SessionStatus> {
        None
    }
    /// Stops that session; `cancel` on an "approved" job calls this.
    async fn stop_session(&self, _job: Value) -> FeatureResult<()> {
        Ok(())
    }
    /// Re-checks and re-renders the job's diff in place: `JobStatus` if the job isn't in a status
    /// this supports, `WorktreeGone` if its worktree no longer exists.
    fn supports_refresh_diff(&self) -> bool {
        false
    }
    async fn refresh_diff(&self, _job: Value, _ctx: FeatureContext) -> FeatureResult<Value> {
        Err(FeatureError::Other("This feature can't refresh its diff.".into()))
    }
    /// Named actions on one of this feature's jobs, served at `POST /features/<id>/:jobId/actions/<name>`.
    /// Anything a body names (a folder, a session) must be checked against the job's own data.
    fn action_names(&self) -> Vec<&'static str> {
        vec![]
    }
    async fn run_action(&self, name: &str, _job: Value, _ctx: FeatureContext) -> FeatureResult<Value> {
        Err(FeatureError::Other(format!("No action \"{name}\".")))
    }
    /// The job as the extension should see it: a feature that hands the page an address re-checks
    /// it here on every read. Must not mutate anything.
    fn present(&self, job: Value) -> Value {
        job
    }
    /// Background work this feature needs while the service runs (a poll); returns how to stop it.
    fn start_background(&self) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }
}

/// The similar-items lookup features may use (`similar_search`); absent when the history is off.
#[async_trait]
pub trait FindSimilar: Send + Sync {
    async fn find(&self, key: Option<String>, text: Option<String>, k: Option<f64>) -> Result<Value, String>;
}

/// What the service hands every feature factory besides the config.
#[derive(Clone)]
pub struct FeatureDeps {
    pub providers: Providers,
    /// The local history, or `None` when it is off (a dev checkout).
    pub history: Option<Arc<HistoryDb>>,
    pub find_similar: Option<Arc<dyn FindSimilar>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn data(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn create_makes_a_running_job_and_get_and_list_return_copies_newest_first() {
        let s = JobStore::new();
        let a = s.create("f", data(json!({ "x": 1 })));
        assert_eq!((a["status"].clone(), a["featureId"].clone(), a["data"]["x"].clone()), (json!("running"), json!("f"), json!(1)));
        assert_eq!(a["createdAt"], a["updatedAt"]);
        let mut copy = s.get(a["id"].as_str().unwrap()).unwrap();
        copy["status"] = json!("failed");
        assert_eq!(s.get(a["id"].as_str().unwrap()).unwrap()["status"], "running", "a copy, not the stored job");
        let b = json!({ "id": uuid_v4(), "featureId": "g", "status": "running", "data": {}, "createdAt": a["createdAt"].as_i64().unwrap() + 5, "updatedAt": 0 });
        s.restore(b);
        assert_eq!(s.list().iter().map(|j| j["featureId"].clone()).collect::<Vec<_>>(), [json!("g"), json!("f")]);
        assert_eq!(s.get("nope"), None);
    }

    #[test]
    fn a_pending_job_carries_the_payload_scope_and_mcp_origin() {
        let s = JobStore::new();
        let j = s.create_pending("f", json!({ "k": 1 }), Some("jira:PROJ-1"));
        assert_eq!((j["status"].clone(), j["data"].clone(), j["scopeKey"].clone(), j["startedVia"].clone()), (json!("pending-start"), json!({ "payload": { "k": 1 } }), json!("jira:PROJ-1"), json!("mcp")));
        assert!(s.create_pending("f", json!(null), None).get("scopeKey").is_none());
        assert_eq!(s.lookup("jira:PROJ-1", "f").unwrap()["id"], j["id"]);
        assert!(s.lookup("jira:PROJ-1", "other").is_none());
    }

    #[test]
    fn update_merges_removes_null_fields_stamps_updated_at_and_rejects_unknown_ids() {
        let s = JobStore::new();
        let j = s.create("f", Map::new());
        let id = j["id"].as_str().unwrap();
        s.set(id, "error", json!("boom")).unwrap();
        let u = s.update(id, data(json!({ "status": "failed", "error": null, "progress": { "stepId": "a", "label": "A" } }))).unwrap();
        assert_eq!((u["status"].clone(), u.get("error").is_none(), u["progress"]["label"].clone()), (json!("failed"), true, json!("A")));
        assert!(u["updatedAt"].as_i64() >= j["updatedAt"].as_i64());
        assert_eq!(s.update("nope", Map::new()).unwrap_err(), "Unknown job id: nope");
        assert_eq!(s.patch_data("nope", Map::new()).unwrap_err(), "Unknown job id: nope");
    }

    #[test]
    fn patch_data_merges_into_data_instead_of_replacing_it() {
        let s = JobStore::new();
        let id = s.create("f", data(json!({ "a": 1, "b": 2 })))["id"].as_str().unwrap().to_string();
        let j = s.patch_data(&id, data(json!({ "b": 3, "c": 4 }))).unwrap();
        assert_eq!(j["data"], json!({ "a": 1, "b": 3, "c": 4 }));
        assert_eq!(s.set_data(&id, "d", json!(true)).unwrap()["data"]["d"], true);
    }

    #[test]
    fn listeners_hear_status_changes_only_and_a_panicking_one_is_ignored() {
        let s = JobStore::new();
        let heard = Arc::new(Mutex::new(vec![]));
        s.on_transition(|_, _| panic!("history exploded"));
        let h = heard.clone();
        s.on_transition(move |job, from| h.lock().unwrap().push(format!("{from}->{}", job["status"].as_str().unwrap())));
        let id = s.create("f", Map::new())["id"].as_str().unwrap().to_string();
        s.set(&id, "progress", json!({ "stepId": "a", "label": "A" })).unwrap();
        s.set(&id, "status", json!("awaiting-approval")).unwrap();
        s.set(&id, "status", json!("awaiting-approval")).unwrap();
        assert_eq!(*heard.lock().unwrap(), ["running->awaiting-approval"]);
    }

    #[test]
    fn progress_is_only_set_while_running_and_active_count_counts_mid_flight_jobs() {
        let s = JobStore::new();
        let id = s.create("f", Map::new())["id"].as_str().unwrap().to_string();
        s.set_progress_if_running(&id, "merge", "Merging");
        assert_eq!(s.get(&id).unwrap()["progress"], json!({ "stepId": "merge", "label": "Merging" }));
        assert_eq!(s.active_count(), 1);
        s.set(&id, "status", json!("rejected")).unwrap();
        s.set_progress_if_running(&id, "late", "Late label");
        assert_eq!(s.get(&id).unwrap()["progress"]["stepId"], "merge", "a late label never overwrites a finished job");
        assert_eq!(s.active_count(), 0);
        s.create_pending("f", json!(null), None);
        assert_eq!(s.active_count(), 0);
        s.set_progress_if_running("nope", "a", "b");
    }

    #[test]
    fn restore_keeps_an_existing_job() {
        let s = JobStore::new();
        let j = s.create("f", Map::new());
        let mut other = j.clone();
        other["status"] = json!("failed");
        assert_eq!(s.restore(other)["status"], "running");
    }

    fn opts<'a>(dir: &'a str, root: &'a str, ids: &[&str]) -> PersistenceOptions<'a> {
        PersistenceOptions { dir, worktree_root: root, feature_ids: ids.iter().map(|s| s.to_string()).collect(), session_cwd_ok: &|_| true, adopted_worktree_ok: &|_, _| true }
    }

    #[test]
    fn persistence_is_write_through_for_opted_in_features_reloads_and_can_be_frozen() {
        let d = tempfile::tempdir().unwrap();
        let (dir, root) = (d.path().join("jobs").to_string_lossy().into_owned(), d.path().join("wt").to_string_lossy().into_owned());
        let s = JobStore::new();
        let off = s.create("persisted", Map::new());
        assert!(job_files::job_file(&dir, off["id"].as_str().unwrap()).is_ok_and(|f| !std::path::Path::new(&f).exists()), "nothing is written until persistence is on");
        assert_eq!(s.enable_persistence(opts(&dir, &root, &["persisted"])).loaded, 0);
        let kept = s.create("persisted", data(json!({ "n": 1 })));
        let skipped = s.create("other", Map::new());
        let file = |j: &Value| std::path::PathBuf::from(job_files::job_file(&dir, j["id"].as_str().unwrap()).unwrap());
        assert!(file(&kept).exists() && !file(&skipped).exists());
        s.set_data(kept["id"].as_str().unwrap(), "n", json!(2)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(file(&kept)).unwrap()).unwrap()["data"]["n"], 2);

        // A second store (a restart) reloads it, with an interrupted "running" job reconciled to failed.
        let s2 = JobStore::new();
        let load = s2.enable_persistence(opts(&dir, &root, &["persisted"]));
        assert_eq!((load.loaded, load.reconciled.len()), (1, 1));
        assert_eq!(s2.get(kept["id"].as_str().unwrap()).unwrap()["status"], "failed");

        // A terminal status removes the file; a frozen store stops writing.
        s.set(kept["id"].as_str().unwrap(), "status", json!("approved")).unwrap();
        assert!(!file(&kept).exists());
        let again = s.create("persisted", Map::new());
        assert!(file(&again).exists());
        s.freeze_persistence();
        s.set(again["id"].as_str().unwrap(), "status", json!("approved")).unwrap();
        assert!(file(&again).exists(), "frozen: the on-disk copy stays as it was");
    }

    #[test]
    fn sweep_stale_drops_only_old_awaiting_or_failed_persisted_jobs_from_memory_and_disk() {
        let d = tempfile::tempdir().unwrap();
        let (dir, root) = (d.path().join("jobs").to_string_lossy().into_owned(), d.path().join("wt").to_string_lossy().into_owned());
        let s = JobStore::new();
        assert!(s.sweep_stale(1.0, 0.0).is_empty(), "off without persistence");
        s.enable_persistence(opts(&dir, &root, &["persisted"]));
        let old = s.create("persisted", Map::new());
        let fresh = s.create("persisted", Map::new());
        let unpersisted = s.create("other", Map::new());
        for j in [&old, &unpersisted] {
            s.set(j["id"].as_str().unwrap(), "status", json!("failed")).unwrap();
        }
        let t = now() as f64;
        let swept = s.sweep_stale(1000.0, t + 5000.0);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0]["id"], old["id"]);
        assert!(s.get(old["id"].as_str().unwrap()).is_none() && s.get(unpersisted["id"].as_str().unwrap()).is_some());
        assert!(s.get(fresh["id"].as_str().unwrap()).is_some(), "a running job is never swept");
        assert!(!std::path::Path::new(&job_files::job_file(&dir, old["id"].as_str().unwrap()).unwrap()).exists());
    }

    #[test]
    fn feature_errors_display_their_message_and_convert_from_strings() {
        let e = FeatureError::refused("Already running.", "already-running", data(json!({ "jobId": "j" })));
        assert_eq!(e.to_string(), "Already running.");
        assert!(matches!(FeatureError::from("x".to_string()), FeatureError::Other(m) if m == "x"));
        assert_eq!(FeatureError::JobStatus("busy".into()).to_string(), "busy");
        assert_eq!(FeatureError::WorktreeGone("gone".into()).to_string(), "gone");
    }

    struct Minimal;
    #[async_trait]
    impl Feature for Minimal {
        fn id(&self) -> &str {
            "min"
        }
        fn label(&self) -> &str {
            "Minimal"
        }
        async fn start(&self, _: Value, _: FeatureContext) -> FeatureResult<Value> {
            Ok(json!({}))
        }
        async fn approve(&self, _: Value, _: FeatureContext) -> FeatureResult<()> {
            Ok(())
        }
        async fn reject(&self, _: Value, _: FeatureContext) -> FeatureResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_minimal_feature_has_none_of_the_optional_operations() {
        let f = Minimal;
        let calls = AtomicUsize::new(0);
        assert!(!f.supports_cancel() && !f.supports_post_comment() && !f.supports_refresh_diff());
        assert!(f.action_names().is_empty() && f.start_background().is_none());
        assert_eq!(f.session_status(&json!({})).await, None);
        assert_eq!(f.present(json!({ "a": 1 })), json!({ "a": 1 }));
        assert!(f.run_action("x", json!({}), FeatureContext::default()).await.is_err());
        calls.fetch_add(1, Ordering::SeqCst);
    }
}
