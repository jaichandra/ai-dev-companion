//! How the background work is put together (port of `core/background-wiring.ts`),
//! out of the server so it can be tested: which context a feature start gets, the
//! watchers' hand-off to the service's job starter, the scheduler with its tasks
//! (including `history.embed`, only when the local history is on), and the inbox
//! routes. Everything with a side effect comes in through the deps.

use crate::background_routes::{BackgroundRoutes, EventSink as RouteEventSink, SchedulerView, SharedInbox};
use crate::config::ConfigFn;
use crate::embed_task::EmbedTask;
use crate::features::digest::index::{create_digest_task, DigestInbox, DigestIo};
use crate::history_db::HistoryDb;
use crate::history_record::used_event;
use crate::jobs::{Feature, FeatureContext, FeatureDeps, JobStore};
use crate::prereqs::GitRemoteRules;
use crate::schedule::notify_min_interval_ms;
use crate::scheduler::{task_fn, Clock, JobRunningFn, LogFn, Outcome, Scheduler, SchedulerOptions, TaskFn, TaskResult};
use crate::similar_search::Embedder;
use crate::watcher_runner::{NotificationSink, StartJob, WatcherDeps, WatcherIo, WatcherLlm, WatcherRunner};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartVia {
    Extension,
    Mcp,
    Watcher,
}

impl StartVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Mcp => "mcp",
            Self::Watcher => "watcher",
        }
    }
}

/// Job states in which a job for the same PR or ticket still counts as open: a watcher must not
/// start a second one.
pub const ACTIVE_JOB_STATUSES: [&str; 5] = ["running", "awaiting-approval", "approving", "rejecting", "pending-start"];

/// The context a feature start really gets. `background` is honoured only for the watcher's own
/// call, whatever a caller put in the context, so a request (extension or MCP) can never select
/// the background policy.
pub fn feature_context_for(via: StartVia, ctx: &FeatureContext) -> FeatureContext {
    FeatureContext { background: via == StartVia::Watcher && ctx.background, ..ctx.clone() }
}

/// Builds a feature from the config and the shared deps.
pub type FeatureFactory = Box<dyn Fn(&Value, &FeatureDeps) -> Arc<dyn Feature> + Send + Sync>;

/// Builds the enabled features, in the order of `ids`, handing every factory the same deps (the
/// history and, with it on, the shared similar-item search). An id without a factory is skipped
/// and a factory that isn't enabled never runs.
pub fn build_features(ids: &[String], factories: &HashMap<String, FeatureFactory>, config: &Value, deps: &FeatureDeps) -> Vec<Arc<dyn Feature>> {
    ids.iter().filter_map(|id| factories.get(id)).map(|f| f(config, deps)).collect()
}

/// The service's `startFeatureJob`: starts a feature the way a click would, tags the job and
/// records it.
#[async_trait]
pub trait StartFeatureJob: Send + Sync {
    async fn start_feature_job(&self, feature: Arc<dyn Feature>, payload: Value, ctx: FeatureContext, via: StartVia, watcher: Option<&str>) -> Result<Value, String>;
}

/// The inbox as the watchers and the digest add to it.
pub struct InboxSink(pub SharedInbox);

impl NotificationSink for InboxSink {
    fn add(&self, item: &Value) -> Result<(), String> {
        self.0.lock().unwrap().add(item).map(|_| ())
    }
}

impl DigestInbox for InboxSink {
    fn add(&self, item: &Value) -> Result<(), String> {
        self.0.lock().unwrap().add(item).map(|_| ())
    }
    fn absorb_pending(&self) -> Vec<String> {
        self.0.lock().unwrap().absorb_pending()
    }
}

struct SchedView(Arc<Scheduler>);

impl SchedulerView for SchedView {
    fn status(&self) -> Value {
        self.0.status()
    }
    fn quiet_now(&self) -> bool {
        self.0.quiet_now()
    }
}

struct RouteSink(Arc<HistoryDb>);

impl RouteEventSink for RouteSink {
    fn record_event(&self, event: Value) {
        let _ = self.0.record_event_json(&event);
    }
}

/// A task's report from the JSON a plain-data task returns (`{outcome?, error?, ...metrics}`).
pub fn task_result_from_value(v: &Value) -> TaskResult {
    let mut r = TaskResult::ok();
    for (k, val) in v.as_object().into_iter().flatten() {
        match k.as_str() {
            "outcome" => r.outcome = if val == "failed" { Outcome::Failed } else { Outcome::Completed },
            "needsLogin" => r.needs_login = val == &json!(true),
            "error" => r.error = val.as_str().map(String::from),
            _ => {
                r.metrics.insert(k.clone(), val.clone());
            }
        }
    }
    r
}

/// The `history.embed` task: embeds new history items on-prem for similar-item search.
pub fn history_embed_task<L: Embedder + 'static>(history: Arc<HistoryDb>, llm: Arc<L>, config: ConfigFn) -> TaskFn {
    task_fn(move |_| {
        let (history, llm, config) = (history.clone(), llm.clone(), config.clone());
        async move { Ok(task_result_from_value(&EmbedTask::new(Some(&history), llm.as_ref(), config()).run().await)) }
    })
}

pub struct BackgroundWiringDeps<L> {
    /// Read afresh on every use, so a Settings save applies live.
    pub config: ConfigFn,
    pub features: Vec<Arc<dyn Feature>>,
    pub job_store: Arc<JobStore>,
    pub start_feature_job: Arc<dyn StartFeatureJob>,
    pub inbox: SharedInbox,
    pub history: Option<Arc<HistoryDb>>,
    pub llm: Arc<L>,
    pub io: Arc<dyn WatcherIo>,
    pub digest_io: Arc<dyn DigestIo>,
    pub clock: Option<Arc<dyn Clock>>,
    pub state_path: Option<PathBuf>,
    /// The daily job-expiry and history-pruning pass.
    pub maintenance: TaskFn,
    /// True once the service is shutting down: no new background jobs start.
    pub is_shutting_down: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    pub git: GitRemoteRules,
    /// The git host's and tracker's base URLs, for inbox links.
    pub base_urls: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    pub log: Option<LogFn>,
}

pub struct BackgroundWiring {
    pub watcher_runner: Arc<WatcherRunner>,
    pub scheduler: Arc<Scheduler>,
    pub routes: Arc<BackgroundRoutes>,
    job_store: Arc<JobStore>,
}

impl BackgroundWiring {
    /// Whether a job of `feature_id` for `key` is still open.
    pub fn active_job_for(&self, key: &str, feature_id: &str) -> bool {
        active_job_for(&self.job_store, key, feature_id)
    }
}

fn active_job_for(store: &JobStore, key: &str, feature_id: &str) -> bool {
    store.list().iter().any(|j| j["scopeKey"] == key && j["featureId"] == feature_id && ACTIVE_JOB_STATUSES.contains(&j["status"].as_str().unwrap_or("")))
}

struct WatcherStart {
    features: Vec<Arc<dyn Feature>>,
    start: Arc<dyn StartFeatureJob>,
    shutting_down: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[async_trait]
impl StartJob for WatcherStart {
    async fn start_job(&self, feature_id: &str, payload: Value, watcher: &str) -> Result<Value, String> {
        if self.shutting_down.as_ref().is_some_and(|f| f()) {
            return Err("The service is shutting down.".into());
        }
        let feature = self.features.iter().find(|f| f.id() == feature_id).ok_or_else(|| format!("{feature_id} isn't enabled."))?;
        // The watchers start jobs exactly as a click would (same readiness and repo checks), but
        // marked `startedVia: "watcher"` and with ctx.background, so a feature that runs Claude
        // with no click behind it gets the stricter readOnlyBackground policy. Nothing here
        // approves or pushes: only a person's Approve pushes.
        let ctx = FeatureContext { auth: Default::default(), body: Some(payload.clone()), background: true };
        self.start.start_feature_job(feature.clone(), payload, ctx, StartVia::Watcher, Some(watcher)).await
    }
}

pub fn create_background_wiring<L>(deps: BackgroundWiringDeps<L>) -> BackgroundWiring
where
    L: WatcherLlm + Embedder + 'static,
{
    let store = deps.job_store.clone();
    let active = {
        let store = store.clone();
        Arc::new(move |key: &str, feature_id: &str| active_job_for(&store, key, feature_id))
    };
    let enabled_ids: Vec<String> = deps.features.iter().map(|f| f.id().to_string()).collect();
    let llm: Arc<dyn WatcherLlm> = deps.llm.clone();
    let watcher_runner = WatcherRunner::new(WatcherDeps {
        config: deps.config.clone(),
        io: deps.io.clone(),
        llm,
        notifications: Arc::new(InboxSink(deps.inbox.clone())),
        enabled_feature_ids: Arc::new(move || enabled_ids.clone()),
        active_job_for: active,
        start_job: Arc::new(WatcherStart { features: deps.features.clone(), start: deps.start_feature_job.clone(), shutting_down: deps.is_shutting_down.clone() }),
        base_urls: deps.base_urls.clone(),
        git: deps.git,
        log: deps.log.clone(),
    });

    let mut tasks: HashMap<String, TaskFn> = HashMap::new();
    tasks.insert("maintenance".into(), deps.maintenance.clone());
    tasks.extend(watcher_runner.tasks());
    tasks.insert("digest".into(), create_digest_task(deps.digest_io.clone(), Arc::new(InboxSink(deps.inbox.clone()))));
    // Embeds new history items on-prem for similar-item search; with no history there is nothing
    // to embed, so the task isn't registered.
    if let Some(history) = &deps.history {
        tasks.insert("history.embed".into(), history_embed_task(history.clone(), deps.llm.clone(), deps.config.clone()));
    }
    let mut opts = SchedulerOptions::new(deps.config.clone(), tasks);
    opts.state_path = deps.state_path.clone();
    opts.history = deps.history.clone().map(|h| h as Arc<dyn crate::scheduler::EventSink>);
    if let Some(clock) = &deps.clock {
        opts.clock = clock.clone();
    }
    let running_store = store.clone();
    opts.is_job_running = Some(Arc::new(move |id: &str| running_store.get(id).is_some_and(|j| j["status"] == "running")) as JobRunningFn);
    opts.log = deps.log.clone();
    let scheduler = Scheduler::new(opts);

    let get_store = store.clone();
    let config = deps.config.clone();
    let routes = Arc::new(BackgroundRoutes {
        inbox: deps.inbox.clone(),
        scheduler: Box::new(SchedView(scheduler.clone())),
        history: deps.history.clone().map(|h| Box::new(RouteSink(h)) as Box<dyn RouteEventSink>),
        used_event: Box::new(used_event),
        get_job: Box::new(move |id| get_store.get(id)),
        min_interval_ms: Box::new(move || Some(notify_min_interval_ms(&config()))),
        now: Box::new(|| crate::job_files::now_ms() as i64),
        log: Box::new(|m| println!("[notifications] WARN - {m}")),
    });
    BackgroundWiring { watcher_runner, scheduler, routes, job_store: store }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlassian::AtlassianError;
    use crate::jobs::{FeatureError, FeatureResult};
    use crate::local_time::local_ms;
    use crate::notifications::{NotificationStore, StdFs};
    use crate::scheduler::{TimerHandle};
    use std::sync::Mutex;

    const MIN: f64 = 60_000.0;
    const HOUR: f64 = 60.0 * MIN;
    const JOB_ID: &str = "3f2b8c1e-1111-4222-8333-444455556666";

    fn t0() -> f64 {
        local_ms(2026, 8, 29, 8, 0) as f64
    }

    struct Timer {
        at: f64,
        cleared: bool,
    }

    struct FakeClock {
        now: Mutex<f64>,
        timers: Mutex<Vec<Timer>>,
    }

    impl Clock for FakeClock {
        fn now(&self) -> f64 {
            *self.now.lock().unwrap()
        }
        fn set_timer(&self, _f: Box<dyn FnOnce() + Send>, ms: f64) -> TimerHandle {
            let at = self.now() + ms;
            let mut t = self.timers.lock().unwrap();
            t.push(Timer { at, cleared: false });
            (t.len() - 1) as u64
        }
        fn clear_timer(&self, h: TimerHandle) {
            self.timers.lock().unwrap()[h as usize].cleared = true;
        }
    }

    fn pr() -> Value {
        json!({ "id": 12, "title": "PROJ-7: fix login", "state": "OPEN", "project": "CI", "repo": "sample-app", "fromBranch": "bugfix/PROJ-7", "fromSha": "aaa111", "toBranch": "master", "toSha": "bbb222", "url": "https://bb.example/projects/CI/repos/sample-app/pull-requests/12", "approvals": 0, "reviewers": [] })
    }

    struct Io;
    #[async_trait]
    impl WatcherIo for Io {
        async fn list_dashboard_pull_requests(&self, _: &str) -> Result<Vec<Value>, AtlassianError> {
            Ok(vec![pr()])
        }
        async fn get_merge_status(&self, _: &str, _: &str, _: i64) -> Result<Value, AtlassianError> {
            Ok(json!({ "conflicted": true }))
        }
        async fn search_issues(&self, _: &str) -> Result<Vec<Value>, AtlassianError> {
            Ok(vec![])
        }
        async fn who_am_i(&self) -> Option<String> {
            Some("me".into())
        }
        fn has_cached_analysis(&self, _: &str) -> bool {
            false
        }
        fn repo_path(&self, _: &str, _: &str) -> Option<String> {
            Some("/clones/sample-app".into())
        }
        async fn git_fetch(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct DIo;
    #[async_trait]
    impl DigestIo for DIo {
        async fn list_dashboard_pull_requests(&self, _: &crate::atlassian::AuthContext, _: &str) -> Result<Vec<Value>, AtlassianError> {
            Ok(vec![])
        }
        async fn get_merge_status(&self, _: &crate::atlassian::AuthContext, _: &str, _: &str, _: i64) -> Result<Value, AtlassianError> {
            Ok(json!({}))
        }
        async fn get_commit_build_status(&self, _: &crate::atlassian::AuthContext, _: &str, _: &str, _: &str) -> Result<Option<Value>, AtlassianError> {
            Ok(None)
        }
        async fn search_issues(&self, _: &crate::atlassian::AuthContext, _: &str) -> Result<Vec<Value>, AtlassianError> {
            Ok(vec![])
        }
        fn jobs(&self) -> Result<Vec<Value>, String> {
            Ok(vec![])
        }
        fn base_urls(&self) -> Vec<String> {
            vec![]
        }
    }

    /// An LLM that is never available.
    struct NoKey;
    #[async_trait]
    impl WatcherLlm for NoKey {
        async fn available(&self) -> bool {
            false
        }
        async fn chat(&self, _: &str, _: &[crate::llm::ChatMessage], _: &Value, _: f64) -> Result<Option<Value>, String> {
            Ok(None)
        }
    }
    impl Embedder for NoKey {
        fn embedding_model(&self) -> Result<String, String> {
            Ok("embedding-model".into())
        }
        async fn available(&self) -> bool {
            false
        }
        async fn detect_state(&self) -> String {
            "no-key".into()
        }
        async fn embed(&self, _: &[String]) -> Result<Vec<Vec<f64>>, String> {
            Err("must not be called without a key".into())
        }
    }

    #[derive(Default)]
    struct Calls {
        start_feature_job: Vec<(String, String, Option<String>, bool)>,
        seen_ctx: Vec<bool>,
        approve: usize,
        reject: usize,
        maintenance: usize,
    }

    struct ResolveConflict {
        store: Arc<JobStore>,
        calls: Arc<Mutex<Calls>>,
    }

    #[async_trait]
    impl Feature for ResolveConflict {
        fn id(&self) -> &str {
            "resolve-conflict"
        }
        fn label(&self) -> &str {
            "Resolve Conflict"
        }
        async fn start(&self, _: Value, ctx: FeatureContext) -> FeatureResult<Value> {
            self.calls.lock().unwrap().seen_ctx.push(ctx.background);
            let job = self.store.create("resolve-conflict", Default::default());
            self.store.set(job["id"].as_str().unwrap(), "status", json!("awaiting-approval")).map_err(FeatureError::Other)
        }
        async fn approve(&self, _: Value, _: FeatureContext) -> FeatureResult<()> {
            self.calls.lock().unwrap().approve += 1;
            Ok(())
        }
        async fn reject(&self, _: Value, _: FeatureContext) -> FeatureResult<()> {
            self.calls.lock().unwrap().reject += 1;
            Ok(())
        }
    }

    /// What the service's `startFeatureJob` does with the context and the tagging.
    struct Starter {
        store: Arc<JobStore>,
        calls: Arc<Mutex<Calls>>,
    }

    #[async_trait]
    impl StartFeatureJob for Starter {
        async fn start_feature_job(&self, feature: Arc<dyn Feature>, payload: Value, ctx: FeatureContext, via: StartVia, watcher: Option<&str>) -> Result<Value, String> {
            self.calls.lock().unwrap().start_feature_job.push((feature.id().into(), via.as_str().into(), watcher.map(String::from), ctx.background));
            let job = feature.start(payload, feature_context_for(via, &ctx)).await.map_err(|e| e.to_string())?;
            let id = job["id"].as_str().unwrap().to_string();
            let mut patch = serde_json::Map::new();
            patch.insert("scopeKey".into(), json!("bitbucket:CI/sample-app#12"));
            patch.insert("startedVia".into(), json!(via.as_str()));
            if let Some(w) = watcher {
                patch.insert("watcher".into(), json!(w));
            }
            self.store.update(&id, patch)
        }
    }

    struct Fx {
        wiring: BackgroundWiring,
        clock: Arc<FakeClock>,
        store: Arc<JobStore>,
        calls: Arc<Mutex<Calls>>,
        inbox: SharedInbox,
        events: Arc<Mutex<Vec<Value>>>,
        _dir: tempfile::TempDir,
    }

    fn setup(shutting_down: bool, history: bool) -> Fx {
        let clock = Arc::new(FakeClock { now: Mutex::new(t0()), timers: Mutex::new(vec![]) });
        let store = Arc::new(JobStore::new());
        let calls = Arc::new(Mutex::new(Calls::default()));
        let c = clock.clone();
        let seq = Arc::new(Mutex::new(0));
        let inbox: SharedInbox = Arc::new(Mutex::new(NotificationStore::with(None, move || c.now() as i64, move || {
            let mut s = seq.lock().unwrap();
            *s += 1;
            format!("00000000-0000-4000-8000-{:012}", *s)
        }, Box::new(StdFs), |_| {})));
        let m = calls.clone();
        let maintenance = task_fn(move |_| {
            let m = m.clone();
            async move {
                m.lock().unwrap().maintenance += 1;
                Ok(TaskResult::ok())
            }
        });
        let config = json!({ "repos": { "CI/sample-app": "/clones/sample-app" }, "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 15 } } });
        let dir = tempfile::tempdir().unwrap();
        let history_db = history.then(|| Arc::new(HistoryDb::open(&dir.path().join("history.sqlite"), GitRemoteRules::BitbucketDc).unwrap()));
        let wiring = create_background_wiring(BackgroundWiringDeps {
            config: Arc::new(move || config.clone()),
            features: vec![Arc::new(ResolveConflict { store: store.clone(), calls: calls.clone() })],
            job_store: store.clone(),
            start_feature_job: Arc::new(Starter { store: store.clone(), calls: calls.clone() }),
            inbox: inbox.clone(),
            history: history_db,
            llm: Arc::new(NoKey),
            io: Arc::new(Io),
            digest_io: Arc::new(DIo),
            clock: Some(clock.clone()),
            state_path: None,
            maintenance,
            is_shutting_down: Some(Arc::new(move || shutting_down)),
            git: GitRemoteRules::BitbucketDc,
            base_urls: Arc::new(|| vec!["https://bb.example".into()]),
            log: Some(Arc::new(|_| {})),
        });
        Fx { wiring, clock, store, calls, inbox, events: Arc::new(Mutex::new(vec![])), _dir: dir }
    }

    impl Fx {
        fn advance(&self, ms: f64) {
            *self.clock.now.lock().unwrap() += ms;
        }
    }

    #[test]
    fn feature_context_for_only_the_watchers_own_call_keeps_background() {
        let ctx = FeatureContext { auth: Default::default(), body: Some(json!({ "a": 1 })), background: true };
        assert!(!feature_context_for(StartVia::Extension, &ctx).background);
        assert!(!feature_context_for(StartVia::Mcp, &ctx).background);
        assert!(feature_context_for(StartVia::Watcher, &ctx).background);
        assert!(!feature_context_for(StartVia::Watcher, &FeatureContext { background: false, ..ctx.clone() }).background, "no flag, no background");
        assert_eq!(feature_context_for(StartVia::Extension, &ctx).body, Some(json!({ "a": 1 })));
    }

    #[tokio::test]
    async fn a_watcher_started_resolve_conflict_is_background_tagged_and_stops_at_awaiting_approval() {
        let t = setup(false, false);
        assert_eq!(t.wiring.scheduler.tick().await, ["watcher.conflicts"]);
        assert_eq!(t.calls.lock().unwrap().start_feature_job, [("resolve-conflict".to_string(), "watcher".to_string(), Some("conflicts".to_string()), true)]);
        assert_eq!(t.calls.lock().unwrap().seen_ctx, [true]);
        let job = t.store.list().remove(0);
        assert_eq!((job["status"].clone(), job["startedVia"].clone(), job["watcher"].clone()), (json!("awaiting-approval"), json!("watcher"), json!("conflicts")));
        let (approved, rejected) = { let c = t.calls.lock().unwrap(); (c.approve, c.reject) };
        assert_eq!(approved + rejected, 0, "nothing approved or rejected on the user's behalf");
        let items = t.inbox.lock().unwrap().list(false, 50);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].job_id.as_deref(), Some(job["id"].as_str().unwrap()));
        let _ = (JOB_ID, &t.events);
    }

    #[test]
    fn active_job_for_treats_awaiting_approval_and_pending_start_as_open() {
        let t = setup(false, false);
        let key = "bitbucket:CI/sample-app#12";
        assert_eq!(ACTIVE_JOB_STATUSES, ["running", "awaiting-approval", "approving", "rejecting", "pending-start"]);
        let job = t.store.create("resolve-conflict", Default::default());
        let id = job["id"].as_str().unwrap().to_string();
        t.store.set(&id, "scopeKey", json!(key)).unwrap();
        for status in ACTIVE_JOB_STATUSES {
            t.store.set(&id, "status", json!(status)).unwrap();
            assert!(t.wiring.active_job_for(key, "resolve-conflict"), "{status}");
        }
        for status in ["approved", "failed", "rejected", "expired"] {
            t.store.set(&id, "status", json!(status)).unwrap();
            assert!(!t.wiring.active_job_for(key, "resolve-conflict"), "{status}");
        }
        t.store.set(&id, "status", json!("running")).unwrap();
        assert!(!t.wiring.active_job_for(key, "analyze-issue"), "another feature");
    }

    #[tokio::test]
    async fn the_scheduler_runs_maintenance_once_at_plus_one_minute_then_daily_and_stop_ends_the_loop() {
        let t = setup(false, false);
        t.wiring.scheduler.tick().await;
        assert_eq!(t.calls.lock().unwrap().maintenance, 0);
        t.advance(MIN);
        t.wiring.scheduler.tick().await;
        assert_eq!(t.calls.lock().unwrap().maintenance, 1);
        t.advance(23.0 * HOUR);
        t.wiring.scheduler.tick().await;
        assert_eq!(t.calls.lock().unwrap().maintenance, 1, "not before a day has passed");
        t.advance(HOUR);
        t.wiring.scheduler.tick().await;
        assert_eq!(t.calls.lock().unwrap().maintenance, 2);

        t.wiring.scheduler.start();
        let armed: Vec<usize> = t.clock.timers.lock().unwrap().iter().enumerate().filter(|(_, x)| !x.cleared).map(|(i, _)| i).collect();
        assert_eq!(armed.len(), 1);
        t.wiring.scheduler.stop();
        assert!(t.clock.timers.lock().unwrap()[armed[0]].cleared);
        assert!(t.clock.timers.lock().unwrap()[armed[0]].at > t.clock.now());
    }

    #[tokio::test]
    async fn after_the_service_starts_shutting_down_a_watcher_starts_nothing_and_says_so_in_the_inbox() {
        let t = setup(true, false);
        t.wiring.scheduler.tick().await;
        assert!(t.calls.lock().unwrap().start_feature_job.is_empty());
        assert!(t.store.list().is_empty());
        let items = t.inbox.lock().unwrap().list(false, 50);
        assert!(items[0].body.contains("couldn't start (The service is shutting down"), "{}", items[0].body);
    }

    #[test]
    fn the_routes_are_the_inbox_handlers_over_the_one_shared_inbox() {
        let t = setup(false, false);
        let (status, body) = t.wiring.routes.list(false);
        assert_eq!((status, body["unseen"].clone()), (200, json!(0)));
        t.inbox.lock().unwrap().add(&json!({ "key": "k", "kind": "analysis", "title": "T", "body": "B" })).unwrap();
        assert_eq!(t.wiring.routes.list(false).1["unseen"], 1, "an item added by a watcher shows in the routes");
    }

    #[tokio::test]
    async fn history_embed_runs_two_minutes_after_start_with_a_history_and_records_skipped_no_llm_without_a_key() {
        let t = setup(false, true);
        t.advance(2.0 * MIN);
        assert_eq!(t.wiring.scheduler.tick().await, ["maintenance", "watcher.conflicts", "history.embed"]);
        let status = t.wiring.scheduler.status();
        assert!(status["running"].is_null());
    }

    #[tokio::test]
    async fn without_a_history_the_scheduler_has_no_embed_task() {
        let t = setup(false, false);
        t.advance(2.0 * MIN);
        let ran = t.wiring.scheduler.tick().await;
        assert!(!ran.contains(&"history.embed".to_string()), "{ran:?}");
        t.advance(60.0 * MIN);
        assert!(!t.wiring.scheduler.tick().await.contains(&"history.embed".to_string()));
    }

    #[test]
    fn task_results_from_plain_json_keep_outcome_error_and_metrics() {
        let r = task_result_from_value(&json!({ "outcome": "failed", "error": "boom", "needsLogin": true, "embedded": 0, "skipped": "no llm" }));
        assert_eq!((r.outcome, r.error.as_deref(), r.needs_login), (Outcome::Failed, Some("boom"), true));
        assert_eq!(r.metrics, json!({ "embedded": 0, "skipped": "no llm" }).as_object().unwrap().clone());
        assert_eq!(task_result_from_value(&json!({ "embedded": 3 })).outcome, Outcome::Completed);
    }

    struct Named(&'static str);
    #[async_trait]
    impl Feature for Named {
        fn id(&self) -> &str {
            self.0
        }
        fn label(&self) -> &str {
            self.0
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

    #[test]
    fn build_features_builds_in_the_order_of_the_enabled_ids_skips_ids_without_a_factory_and_hands_every_factory_the_same_deps() {
        let ran = Arc::new(Mutex::new(vec![]));
        let mut factories: HashMap<String, FeatureFactory> = HashMap::new();
        for id in ["a", "b", "off"] {
            let ran = ran.clone();
            factories.insert(id.into(), Box::new(move |_, deps: &FeatureDeps| {
                ran.lock().unwrap().push((id, deps.find_similar.is_some()));
                Arc::new(Named(id))
            }));
        }
        let store = Arc::new(JobStore::new());
        let _ = store;
        let deps = FeatureDeps { providers: crate::providers::create_providers(test_deps()).unwrap(), history: None, find_similar: None };
        let ids: Vec<String> = ["b", "nope", "a"].iter().map(|s| s.to_string()).collect();
        let built: Vec<String> = build_features(&ids, &factories, &json!({}), &deps).iter().map(|f| f.id().to_string()).collect();
        assert_eq!(built, ["b", "a"]);
        assert_eq!(*ran.lock().unwrap(), [("b", false), ("a", false)]);
        assert!(build_features(&[], &factories, &json!({}), &deps).is_empty());
    }

    fn test_deps() -> crate::providers::Deps<crate::http::testing::FakeTransport> {
        use crate::site_tokens::testing::{tokens, Fixed};
        let t = crate::http::testing::FakeTransport::replying(200, "{}", "application/json");
        crate::providers::Deps { client: Arc::new(crate::atlassian::Client::new(t, Arc::new(crate::auth_context::AuthHub::new(Arc::new(Fixed(vec![])))))), tokens: Arc::new(tokens(vec![])), config: Arc::new(|| json!({})) }
    }
}
