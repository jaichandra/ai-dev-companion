//! The background scheduler (port of `core/scheduler.ts`): one loop that runs the
//! daily maintenance (job expiry and history pruning), the opt-in watchers and
//! the weekday digest, one task at a time. `schedule` decides what's due; this
//! file keeps the state (the day's Claude budget, what each watcher has seen) and
//! records every run as an `events` row. The clock and timers are injected so
//! tests never wait on real time.

use crate::config::ConfigFn;
use crate::history_db::HistoryDb;
use crate::history_record::redact_secrets;
use crate::ids::uuid_v4;
use crate::js_text::slice_utf16;
use crate::schedule::{budget_limit, budget_usage, due_tasks, grant_claude_run, in_quiet_hours, local_day_key, next_wake_delay, normalize_state, quiet_hours_from, refund_claude_run, watcher_settings, BudgetUsage, Ctx, SchedState, WATCHERS};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed => "failed",
        }
    }
}

/// What a task hands back: metrics for its events row, plus how it went.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskResult {
    pub outcome: Outcome,
    /// True when the task couldn't sign in anywhere (no session, no token).
    pub needs_login: bool,
    pub error: Option<String>,
    pub metrics: Map<String, Value>,
}

impl TaskResult {
    pub fn ok() -> Self {
        Self { outcome: Outcome::Completed, needs_login: false, error: None, metrics: Map::new() }
    }
    pub fn failed(error: &str) -> Self {
        Self { outcome: Outcome::Failed, error: Some(error.into()), ..Self::ok() }
    }
    pub fn needs_login(mut self) -> Self {
        self.needs_login = true;
        self
    }
    pub fn metric(mut self, key: &str, value: Value) -> Self {
        self.metrics.insert(key.into(), value);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeRunGrant {
    pub granted: bool,
    pub reason: Option<&'static str>,
    pub usage: BudgetUsage,
}

/// What a task may ask of the scheduler while it runs (a trait, so a watcher's tests can stand in
/// for the scheduler).
pub trait TaskHost: Send + Sync {
    fn now(&self) -> f64;
    fn request_claude_run(&self) -> ClaudeRunGrant;
    fn refund_claude_run(&self);
    fn track_background_job(&self, job_id: &str);
    fn seen(&self, watcher: &str) -> Map<String, Value>;
    fn set_seen(&self, watcher: &str, seen: Map<String, Value>);
}

#[derive(Clone)]
pub struct TaskContext {
    host: Arc<dyn TaskHost>,
    pub quiet: bool,
}

impl TaskContext {
    pub fn new(host: Arc<dyn TaskHost>, quiet: bool) -> Self {
        Self { host, quiet }
    }
    pub fn now(&self) -> f64 {
        self.host.now()
    }
    pub fn request_claude_run(&self) -> ClaudeRunGrant {
        self.host.request_claude_run()
    }
    /// The run just granted never started: give it back.
    pub fn refund_claude_run(&self) {
        self.host.refund_claude_run();
    }
    pub fn track_background_job(&self, job_id: &str) {
        self.host.track_background_job(job_id);
    }
    pub fn seen(&self, watcher: &str) -> Map<String, Value> {
        self.host.seen(watcher)
    }
    pub fn set_seen(&self, watcher: &str, seen: Map<String, Value>) {
        self.host.set_seen(watcher, seen);
    }
}

impl TaskHost for Scheduler {
    fn now(&self) -> f64 {
        self.clock.now()
    }
    fn request_claude_run(&self) -> ClaudeRunGrant {
        Scheduler::request_claude_run(self)
    }
    fn refund_claude_run(&self) {
        Scheduler::refund_claude_run(self)
    }
    fn track_background_job(&self, job_id: &str) {
        Scheduler::track_background_job(self, job_id)
    }
    fn seen(&self, watcher: &str) -> Map<String, Value> {
        self.lock().sched.seen.get(watcher).and_then(Value::as_object).cloned().unwrap_or_default()
    }
    fn set_seen(&self, watcher: &str, seen: Map<String, Value>) {
        let mut st = self.lock();
        st.sched.seen.insert(watcher.into(), Value::Object(seen));
        self.save(&st);
    }
}

/// Whether a job id is still running.
pub type JobRunningFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;
/// A log sink.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

pub type TaskFn = Arc<dyn Fn(TaskContext) -> BoxFuture<Result<TaskResult, String>> + Send + Sync>;

/// Wraps an async closure as a task.
pub fn task_fn<F, Fut>(f: F) -> TaskFn
where
    F: Fn(TaskContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<TaskResult, String>> + Send + 'static,
{
    Arc::new(move |ctx| Box::pin(f(ctx)))
}

pub type TimerHandle = u64;

pub trait Clock: Send + Sync {
    fn now(&self) -> f64;
    fn set_timer(&self, f: Box<dyn FnOnce() + Send>, ms: f64) -> TimerHandle;
    fn clear_timer(&self, handle: TimerHandle);
}

/// Wall-clock time and tokio timers.
#[derive(Default)]
pub struct RealClock {
    next: Mutex<u64>,
    timers: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
}

impl Clock for RealClock {
    fn now(&self) -> f64 {
        crate::job_files::now_ms()
    }
    fn set_timer(&self, f: Box<dyn FnOnce() + Send>, ms: f64) -> TimerHandle {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        let id = *next;
        let h = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms.max(0.0) as u64)).await;
            f();
        });
        self.timers.lock().unwrap().insert(id, h.abort_handle());
        id
    }
    fn clear_timer(&self, handle: TimerHandle) {
        if let Some(h) = self.timers.lock().unwrap().remove(&handle) {
            h.abort();
        }
    }
}

/// Where a run is recorded (`HistoryDb`).
pub trait EventSink: Send + Sync {
    fn record_event(&self, event: &Value) -> Result<(), String>;
}

impl EventSink for HistoryDb {
    fn record_event(&self, event: &Value) -> Result<(), String> {
        self.record_event_json(event)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskStatus {
    pub at: f64,
    pub outcome: Outcome,
    pub duration_ms: f64,
    pub needs_login: bool,
    pub error: Option<String>,
}

impl TaskStatus {
    fn to_json(&self) -> Value {
        let mut v = json!({ "at": self.at, "outcome": self.outcome.as_str(), "durationMs": self.duration_ms, "needsLogin": self.needs_login });
        if let Some(e) = &self.error {
            v["error"] = json!(e);
        }
        v
    }
}

pub struct SchedulerOptions {
    /// Read afresh on every use, so a Settings change applies live.
    pub config: ConfigFn,
    pub tasks: HashMap<String, TaskFn>,
    /// Where the state is kept (state dir / scheduler.json); `None` keeps it in memory.
    pub state_path: Option<PathBuf>,
    pub history: Option<Arc<dyn EventSink>>,
    pub clock: Arc<dyn Clock>,
    /// Whether a job id is still running (`jobStore.get(id)?.status == "running"`).
    pub is_job_running: Option<JobRunningFn>,
    pub log: Option<LogFn>,
}

impl SchedulerOptions {
    pub fn new(config: ConfigFn, tasks: HashMap<String, TaskFn>) -> Self {
        Self { config, tasks, state_path: None, history: None, clock: Arc::new(RealClock::default()), is_job_running: None, log: None }
    }
}

struct State {
    sched: SchedState,
    timer: Option<TimerHandle>,
    ticking: bool,
    stopped: bool,
    /// `stop` was called (and `start` hasn't been since): a running tick starts no further task.
    halted: bool,
    running: Option<String>,
    background_job_id: Option<String>,
    last_status: HashMap<String, TaskStatus>,
}

pub struct Scheduler {
    opts: SchedulerOptions,
    clock: Arc<dyn Clock>,
    started_at: f64,
    state: Mutex<State>,
}

fn clip(s: &str) -> String {
    slice_utf16(&redact_secrets(s), 500)
}

impl Scheduler {
    pub fn new(opts: SchedulerOptions) -> Arc<Self> {
        let clock = opts.clock.clone();
        let now = clock.now();
        let raw = opts.state_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(Value::Null);
        let sched = normalize_state(&raw, now);
        Arc::new(Self { opts, clock, started_at: now, state: Mutex::new(State { sched, timer: None, ticking: false, stopped: true, halted: false, running: None, background_job_id: None, last_status: HashMap::new() }) })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, message: &str) {
        match &self.opts.log {
            Some(f) => f(message),
            None => println!("[scheduler] {message}"),
        }
    }

    fn save(&self, st: &State) {
        let Some(file) = &self.opts.state_path else { return };
        let write = || -> std::io::Result<()> {
            let dir = file.parent().expect("a state file has a folder");
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            let tmp = dir.join(format!(".{}.{}.tmp", file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), std::process::id()));
            let result = (|| {
                let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
                f.write_all((serde_json::to_string_pretty(&st.sched.to_json()).expect("state serializes") + "\n").as_bytes())?;
                drop(f);
                std::fs::rename(&tmp, file)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result
        };
        if let Err(e) = write() {
            self.log(&format!("WARN - couldn't save the scheduler state: {e}"));
        }
    }

    fn with_ctx<T>(&self, st: &State, f: impl FnOnce(&Ctx<'_>) -> T) -> T {
        let config = (self.opts.config)();
        let registered: Vec<&str> = self.opts.tasks.keys().map(String::as_str).collect();
        f(&Ctx { config: &config, state: &st.sched, now: self.clock.now(), started_at: self.started_at, registered: Some(&registered) })
    }

    pub fn quiet_now(&self) -> bool {
        in_quiet_hours(self.clock.now(), quiet_hours_from(&(self.opts.config)()))
    }

    /// Starts the loop: a tick whenever something is due, at least once a minute.
    pub fn start(self: &Arc<Self>) {
        let delay = {
            let mut st = self.lock();
            st.stopped = false;
            st.halted = false;
            self.with_ctx(&st, next_wake_delay)
        };
        self.arm(delay);
    }

    pub fn stop(&self) {
        let mut st = self.lock();
        st.stopped = true;
        st.halted = true;
        if let Some(t) = st.timer.take() {
            self.clock.clear_timer(t);
        }
    }

    fn arm(self: &Arc<Self>, ms: f64) {
        let mut st = self.lock();
        if st.stopped {
            return;
        }
        if let Some(t) = st.timer.take() {
            self.clock.clear_timer(t);
        }
        let me = self.clone();
        st.timer = Some(self.clock.set_timer(
            Box::new(move || {
                me.lock().timer = None;
                let me2 = me.clone();
                tokio::spawn(async move {
                    me2.tick().await;
                    let delay = {
                        let st = me2.lock();
                        me2.with_ctx(&st, next_wake_delay)
                    };
                    me2.arm(delay);
                });
            }),
            ms,
        ));
    }

    /// Runs every task that is due, one at a time. A tick while one is still going does nothing.
    /// Resolves with the names it ran.
    pub async fn tick(self: &Arc<Self>) -> Vec<String> {
        let due = {
            let mut st = self.lock();
            if st.ticking {
                return vec![];
            }
            st.ticking = true;
            self.with_ctx(&st, due_tasks)
        };
        let mut ran = vec![];
        for task in due {
            // stop() during a task (shutdown) means no further task starts.
            if self.lock().halted {
                break;
            }
            // A task that isn't registered (history.embed without a history) is not run, not
            // recorded, and not reported as run.
            let Some(f) = self.opts.tasks.get(task).cloned() else { continue };
            self.run_task(task, f).await;
            ran.push(task.to_string());
        }
        self.lock().ticking = false;
        ran
    }

    async fn run_task(self: &Arc<Self>, task: &str, f: TaskFn) {
        let started = self.clock.now();
        let ctx = {
            // Marked before running, so a failing task waits for its next turn.
            let mut st = self.lock();
            st.sched.set_last_run(task, started);
            if task == "digest" {
                st.sched.last_digest_day = Some(local_day_key(started));
            }
            self.save(&st);
            st.running = Some(task.to_string());
            TaskContext::new(self.clone(), self.quiet_now())
        };
        let result = f(ctx).await.unwrap_or_else(|e| TaskResult::failed(if e.is_empty() { "Error" } else { &e }));
        let ended = self.clock.now();
        let TaskResult { outcome, needs_login, error, metrics } = result;
        let clean_error = error.as_deref().map(clip);
        let metrics: Map<String, Value> = metrics.into_iter().map(|(k, v)| (k, if let Value::String(s) = &v { json!(clip(s)) } else { v })).collect();
        let status = TaskStatus { at: ended, outcome, duration_ms: ended - started, needs_login, error: clean_error.clone() };
        {
            let mut st = self.lock();
            st.running = None;
            st.last_status.insert(task.to_string(), status);
        }
        if outcome == Outcome::Failed {
            self.log(&format!("WARN - {task} failed: {}", clean_error.as_deref().unwrap_or("no details")));
        }
        if let Some(history) = &self.opts.history {
            let mut m = Map::new();
            m.insert("task".into(), json!(task));
            m.extend(metrics);
            if needs_login {
                m.insert("needsLogin".into(), json!(true));
            }
            if let Some(e) = &clean_error {
                m.insert("error".into(), json!(e));
            }
            let event = json!({
                "jobId": format!("sched:{task}:{}", uuid_v4()),
                "featureId": format!("scheduler:{task}"),
                "status": outcome.as_str(),
                "at": ended as i64,
                "durationMs": ended - started,
                "outcome": outcome.as_str(),
                "metrics": m,
            });
            if let Err(e) = history.record_event(&event) {
                self.log(&format!("WARN - couldn't record the {task} run: {e}"));
            }
        }
        self.save(&self.lock());
    }

    /// One more background Claude run? Only one at a time, none in quiet hours, and at most
    /// `budget.claudeRunsPerDay` a day.
    pub fn request_claude_run(&self) -> ClaudeRunGrant {
        let config = (self.opts.config)();
        let mut st = self.lock();
        let busy = match (&st.background_job_id, &self.opts.is_job_running) {
            (Some(id), Some(running)) => running(id),
            _ => false,
        };
        let quiet = in_quiet_hours(self.clock.now(), quiet_hours_from(&config));
        let r = grant_claude_run(&st.sched, self.clock.now(), budget_limit(&config), busy, quiet);
        if r.granted {
            st.sched = r.state;
            self.save(&st);
        }
        ClaudeRunGrant { granted: r.granted, reason: r.reason, usage: r.usage }
    }

    pub fn refund_claude_run(&self) {
        let mut st = self.lock();
        st.sched = refund_claude_run(&st.sched, self.clock.now());
        self.save(&st);
    }

    /// The background run just started: the next one waits until it ends.
    pub fn track_background_job(&self, job_id: &str) {
        self.lock().background_job_id = Some(job_id.into());
    }

    /// What Settings, doctor and `GET /notifications` show.
    pub fn status(&self) -> Value {
        let config = (self.opts.config)();
        let now = self.clock.now();
        let st = self.lock();
        let mut watchers = Map::new();
        for (name, w) in watcher_settings(&config) {
            let last = st.last_status.get(&format!("watcher.{name}")).map_or(Value::Null, TaskStatus::to_json);
            watchers.insert(name.to_string(), json!({ "enabled": w.enabled, "intervalMinutes": w.interval_minutes, "last": last }));
        }
        let b = budget_usage(&st.sched, now, budget_limit(&config));
        json!({
            "running": st.running,
            "quiet": in_quiet_hours(now, quiet_hours_from(&config)),
            "budget": { "day": b.day, "used": b.used, "limit": b.limit, "remaining": b.remaining },
            "watchers": watchers,
            "digest": st.last_status.get("digest").map_or(Value::Null, TaskStatus::to_json),
        })
    }
}

/// The watchers' names, for callers that register one task per watcher.
pub fn watcher_task_names() -> Vec<String> {
    WATCHERS.iter().map(|w| format!("watcher.{w}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_time::local_ms;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MIN: f64 = 60_000.0;

    fn tue_0800() -> f64 {
        local_ms(2026, 8, 29, 8, 0) as f64
    }

    struct Timer {
        f: Option<Box<dyn FnOnce() + Send>>,
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
        fn set_timer(&self, f: Box<dyn FnOnce() + Send>, ms: f64) -> TimerHandle {
            let at = self.now() + ms;
            let mut t = self.timers.lock().unwrap();
            t.push(Timer { f: Some(f), at, cleared: false });
            (t.len() - 1) as u64
        }
        fn clear_timer(&self, handle: TimerHandle) {
            self.timers.lock().unwrap()[handle as usize].cleared = true;
        }
    }

    struct Fx {
        s: Arc<Scheduler>,
        clock: Arc<FakeClock>,
        events: Arc<Mutex<Vec<Value>>>,
        logs: Arc<Mutex<Vec<String>>>,
        config: Arc<Mutex<Value>>,
    }

    struct Sink(Arc<Mutex<Vec<Value>>>);
    impl EventSink for Sink {
        fn record_event(&self, e: &Value) -> Result<(), String> {
            self.0.lock().unwrap().push(e.clone());
            Ok(())
        }
    }

    impl Fx {
        fn advance(&self, ms: f64) {
            *self.clock.now.lock().unwrap() += ms;
        }
        fn live_timers(&self) -> usize {
            self.clock.timers.lock().unwrap().iter().filter(|t| !t.cleared).count()
        }
        fn log_has(&self, needle: &str) -> bool {
            self.logs.lock().unwrap().iter().any(|m| m.contains(needle))
        }
    }

    fn make(config: Value, tasks: Vec<(&str, TaskFn)>, state_path: Option<PathBuf>, is_job_running: Option<JobRunningFn>) -> Fx {
        let clock = Arc::new(FakeClock { now: Mutex::new(tue_0800()), timers: Mutex::new(vec![]) });
        let events = Arc::new(Mutex::new(vec![]));
        let logs = Arc::new(Mutex::new(vec![]));
        let config = Arc::new(Mutex::new(config));
        let c = config.clone();
        let l = logs.clone();
        let mut opts = SchedulerOptions::new(Arc::new(move || c.lock().unwrap().clone()), tasks.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        opts.state_path = state_path;
        opts.history = Some(Arc::new(Sink(events.clone())));
        opts.clock = clock.clone();
        opts.is_job_running = is_job_running;
        opts.log = Some(Arc::new(move |m| l.lock().unwrap().push(m.to_string())));
        Fx { s: Scheduler::new(opts), clock, events, logs, config }
    }

    fn quick(config: Value, tasks: Vec<(&str, TaskFn)>) -> Fx {
        make(config, tasks, None, None)
    }

    fn noop() -> TaskFn {
        task_fn(|_| async { Ok(TaskResult::ok()) })
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn maintenance_runs_a_minute_after_start_an_enabled_watcher_at_once_and_each_run_is_an_events_row() {
        let ran = Arc::new(Mutex::new(vec![]));
        let (r1, r2) = (ran.clone(), ran.clone());
        let t = quick(
            json!({ "similar": { "enabled": false }, "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 15 } } }),
            vec![
                ("maintenance", task_fn(move |_| { let r = r1.clone(); async move { r.lock().unwrap().push("maintenance"); Ok(TaskResult::ok().metric("pruned", json!(2))) } })),
                ("watcher.conflicts", task_fn(move |_| { let r = r2.clone(); async move { r.lock().unwrap().push("conflicts"); Ok(TaskResult::ok().metric("found", json!(1))) } })),
            ],
        );
        assert_eq!(t.s.tick().await, ["watcher.conflicts"]);
        t.advance(MIN);
        assert_eq!(t.s.tick().await, ["maintenance"]);
        t.advance(5.0 * MIN);
        assert!(t.s.tick().await.is_empty());
        t.advance(10.0 * MIN);
        assert_eq!(t.s.tick().await, ["watcher.conflicts"]);
        assert_eq!(*ran.lock().unwrap(), ["conflicts", "maintenance", "conflicts"]);
        let events = t.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        let id = events[0]["jobId"].as_str().unwrap();
        assert!(id.starts_with("sched:watcher.conflicts:") && id.len() == "sched:watcher.conflicts:".len() + 36, "{id}");
        assert_eq!((events[0]["featureId"].clone(), events[0]["outcome"].clone()), (json!("scheduler:watcher.conflicts"), json!("completed")));
        assert_eq!(events[0]["metrics"], json!({ "task": "watcher.conflicts", "found": 1 }));
        assert_eq!(events[1]["metrics"]["pruned"], 2);
    }

    #[tokio::test]
    async fn one_task_at_a_time_a_tick_while_another_is_running_does_nothing() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
        let t = quick(json!({ "watchers": { "conflicts": { "enabled": true } } }), vec![("watcher.conflicts", task_fn(move |_| { let rx = rx.clone(); async move { let r = rx.lock().await.take().unwrap(); let _ = r.await; Ok(TaskResult::ok()) } }))]);
        let s = t.s.clone();
        let first = tokio::spawn(async move { s.tick().await });
        settle().await;
        assert_eq!(t.s.status()["running"], "watcher.conflicts");
        assert!(t.s.tick().await.is_empty());
        tx.send(()).unwrap();
        assert_eq!(first.await.unwrap(), ["watcher.conflicts"]);
        assert_eq!(t.s.status()["running"], Value::Null);
    }

    #[tokio::test]
    async fn a_failing_task_is_recorded_as_failed_with_a_redacted_error_and_needs_login_shows_in_status() {
        let t = quick(
            json!({ "watchers": { "conflicts": { "enabled": true }, "assignedBugs": { "enabled": true } } }),
            vec![
                ("watcher.conflicts", task_fn(|_| async { Err("GET https://u:secret@bb/x failed: Bearer abc123".to_string()) })),
                ("watcher.assignedBugs", task_fn(|_| async { Ok(TaskResult::failed("Jira: no session").needs_login()) })),
            ],
        );
        assert_eq!(t.s.tick().await, ["watcher.conflicts", "watcher.assignedBugs"]);
        let events = t.events.lock().unwrap().clone();
        assert_eq!(events[0]["outcome"], "failed");
        assert_eq!(events[0]["metrics"]["error"], "GET https://***@bb/x failed: Bearer ***");
        assert_eq!(events[1]["metrics"]["needsLogin"], true);
        let status = t.s.status();
        assert_eq!(status["watchers"]["conflicts"]["last"]["outcome"], "failed");
        assert_eq!(status["watchers"]["assignedBugs"]["last"]["needsLogin"], true);
        assert_eq!(status["watchers"]["reviewRequests"]["last"], Value::Null);
        assert!(t.log_has("watcher.conflicts failed"));
    }

    #[test]
    fn request_claude_run_the_daily_budget_one_background_run_at_a_time_none_in_quiet_hours() {
        let running = Arc::new(Mutex::new(true));
        let r = running.clone();
        let t = make(json!({ "budget": { "claudeRunsPerDay": 2 } }), vec![], None, Some(Arc::new(move |_| *r.lock().unwrap())));
        let first = t.s.request_claude_run();
        assert!(first.granted);
        assert_eq!(first.usage, BudgetUsage { day: "2026-09-29".into(), used: 1, limit: 2, remaining: 1 });
        t.s.track_background_job("job-1");
        assert_eq!(t.s.request_claude_run().reason, Some("busy"));
        *running.lock().unwrap() = false;
        assert!(t.s.request_claude_run().granted);
        assert_eq!(t.s.request_claude_run().reason, Some("budget"));
        assert_eq!(t.s.status()["budget"], json!({ "day": "2026-09-29", "used": 2, "limit": 2, "remaining": 0 }));

        let quiet = quick(json!({ "scheduler": { "quietHours": { "start": "07:00", "end": "09:00" } } }), vec![]);
        assert_eq!(quiet.s.request_claude_run().reason, Some("quiet"));
        assert_eq!(quiet.s.status()["quiet"], true);
        let zero = quick(json!({ "budget": { "claudeRunsPerDay": 0 } }), vec![]);
        assert_eq!(zero.s.request_claude_run().reason, Some("budget"));
    }

    #[tokio::test]
    async fn settings_apply_live_a_watcher_turned_on_later_runs_on_the_next_tick() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let t = quick(json!({}), vec![("watcher.reviewRequests", task_fn(move |_| { let r = r.clone(); async move { r.fetch_add(1, Ordering::SeqCst); Ok(TaskResult::ok()) } }))]);
        assert!(t.s.tick().await.is_empty());
        *t.config.lock().unwrap() = json!({ "watchers": { "reviewRequests": { "enabled": true } } });
        assert_eq!(t.s.tick().await, ["watcher.reviewRequests"]);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_budget_and_what_watchers_have_seen_survive_a_restart_and_the_file_is_0600() {
        let d = tempfile::tempdir().unwrap();
        let state_path = d.path().join("scheduler.json");
        let config = json!({ "watchers": { "conflicts": { "enabled": true } } });
        let tasks = || vec![("watcher.conflicts", task_fn(|ctx: TaskContext| async move {
            let mut seen = ctx.seen("conflicts");
            seen.insert("bitbucket:CI/sample-app#1".into(), json!("a:b"));
            ctx.set_seen("conflicts", seen);
            assert!(ctx.request_claude_run().granted);
            Ok(TaskResult::ok())
        }))];
        let first = make(config.clone(), tasks(), Some(state_path.clone()), None);
        first.s.tick().await;
        assert_eq!(std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777, 0o600);
        let again = make(config, vec![], Some(state_path.clone()), None);
        assert_eq!(again.s.status()["budget"]["used"], 1);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(saved["seen"]["conflicts"], json!({ "bitbucket:CI/sample-app#1": "a:b" }));
        assert!(saved["lastRun"]["watcher.conflicts"].is_number());
    }

    #[tokio::test]
    async fn the_digest_runs_once_on_a_weekday_after_its_time() {
        let digests = Arc::new(AtomicUsize::new(0));
        let d = digests.clone();
        let t = quick(json!({ "digest": { "enabled": true, "time": "08:30" } }), vec![("digest", task_fn(move |_| { let d = d.clone(); async move { d.fetch_add(1, Ordering::SeqCst); Ok(TaskResult::ok()) } }))]);
        t.advance(29.0 * MIN);
        t.s.tick().await;
        assert_eq!(digests.load(Ordering::SeqCst), 0);
        t.advance(MIN);
        t.s.tick().await;
        t.s.tick().await;
        assert_eq!(digests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_arms_a_timer_from_the_injected_clock_firing_it_ticks_and_re_arms_stop_clears_it() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let t = quick(json!({ "watchers": { "conflicts": { "enabled": true } } }), vec![("watcher.conflicts", task_fn(move |_| { let r = r.clone(); async move { r.fetch_add(1, Ordering::SeqCst); Ok(TaskResult::ok()) } }))]);
        t.s.start();
        assert_eq!(t.clock.timers.lock().unwrap().len(), 1);
        assert_eq!(t.clock.timers.lock().unwrap()[0].at - tue_0800(), 1000.0, "something is due now: wake in a second");
        let f = t.clock.timers.lock().unwrap()[0].f.take().unwrap();
        f();
        for _ in 0..100 {
            if t.clock.timers.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(t.clock.timers.lock().unwrap().len(), 2, "re-armed after the tick");
        t.s.stop();
        assert!(t.clock.timers.lock().unwrap()[1].cleared);
    }

    #[tokio::test]
    async fn quiet_hours_an_enabled_watcher_does_not_make_the_loop_spin_one_timer_sleeps_to_the_next_wake() {
        let t = quick(json!({ "watchers": { "conflicts": { "enabled": true } }, "scheduler": { "quietHours": { "start": "07:00", "end": "09:00" } } }), vec![("watcher.conflicts", noop())]);
        t.s.start();
        assert_eq!(t.clock.timers.lock().unwrap().len(), 1);
        assert!(t.clock.timers.lock().unwrap()[0].at - tue_0800() >= MIN, "not the one-second busy-wait of a due watcher");
        assert!(t.s.tick().await.is_empty(), "the watcher is held during quiet hours");
        t.s.stop();
    }

    #[tokio::test]
    async fn stop_during_a_task_the_rest_of_that_ticks_tasks_do_not_start() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
        let ran = Arc::new(Mutex::new(vec![]));
        let r = ran.clone();
        let t = quick(
            json!({ "watchers": { "conflicts": { "enabled": true }, "assignedBugs": { "enabled": true } } }),
            vec![
                ("watcher.conflicts", task_fn(move |_| { let rx = rx.clone(); async move { let r = rx.lock().await.take().unwrap(); let _ = r.await; Ok(TaskResult::ok()) } })),
                ("watcher.assignedBugs", task_fn(move |_| { let r = r.clone(); async move { r.lock().unwrap().push("assignedBugs"); Ok(TaskResult::ok()) } })),
            ],
        );
        let s = t.s.clone();
        let tick = tokio::spawn(async move { s.tick().await });
        settle().await;
        t.s.stop();
        tx.send(()).unwrap();
        assert_eq!(tick.await.unwrap(), ["watcher.conflicts"]);
        assert!(ran.lock().unwrap().is_empty());
        t.s.start();
        assert_eq!(t.s.tick().await, ["watcher.assignedBugs"], "start lets ticks run tasks again");
        t.s.stop();
    }

    #[tokio::test]
    async fn string_metrics_are_redacted_and_clipped_before_they_are_recorded() {
        let t = quick(json!({ "watchers": { "conflicts": { "enabled": true } } }), vec![("watcher.conflicts", task_fn(|_| async { Ok(TaskResult::ok().metric("note", json!("GET https://u:secret@bb/x with Bearer abc123")).metric("count", json!(3)).metric("long", json!("x".repeat(900)))) }))]);
        t.s.tick().await;
        let m = t.events.lock().unwrap()[0]["metrics"].clone();
        assert_eq!(m["note"], "GET https://***@bb/x with Bearer ***");
        assert_eq!(m["count"], 3);
        assert_eq!(m["long"].as_str().unwrap().len(), 500);
    }

    #[tokio::test]
    async fn a_state_file_that_cannot_be_written_is_logged_not_thrown_and_the_run_still_counts() {
        let d = tempfile::tempdir().unwrap();
        let blocker = d.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let t = make(json!({ "watchers": { "conflicts": { "enabled": true } } }), vec![("watcher.conflicts", task_fn(move |_| { let r = r.clone(); async move { r.fetch_add(1, Ordering::SeqCst); Ok(TaskResult::ok()) } }))], Some(blocker.join("sub").join("scheduler.json")), None);
        assert_eq!(t.s.tick().await, ["watcher.conflicts"]);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(t.log_has("couldn't save the scheduler state"));
    }

    #[test]
    fn the_claude_budget_starts_over_at_local_midnight() {
        let t = quick(json!({ "budget": { "claudeRunsPerDay": 1 } }), vec![]);
        assert!(t.s.request_claude_run().granted);
        assert_eq!(t.s.request_claude_run().reason, Some("budget"));
        t.advance(16.0 * 60.0 * MIN + 1.0); // 08:00 + 16h -> just past midnight
        let next = t.s.request_claude_run();
        assert!(next.granted);
        assert_eq!(next.usage.used, 1);
    }

    #[tokio::test]
    async fn stop_then_start_leaves_exactly_one_live_timer() {
        let t = quick(json!({ "watchers": { "conflicts": { "enabled": true } } }), vec![("watcher.conflicts", noop())]);
        t.s.start();
        t.s.stop();
        t.s.start();
        assert_eq!(t.live_timers(), 1);
        t.s.stop();
        assert_eq!(t.live_timers(), 0);
    }

    #[test]
    fn refund_claude_run_gives_back_one_run_today_never_below_zero_and_not_from_a_past_day() {
        let t = quick(json!({ "budget": { "claudeRunsPerDay": 2 } }), vec![]);
        t.s.request_claude_run();
        t.s.request_claude_run();
        assert_eq!(t.s.request_claude_run().reason, Some("budget"));
        t.s.refund_claude_run();
        assert_eq!(t.s.status()["budget"]["used"], 1);
        assert!(t.s.request_claude_run().granted);
        for _ in 0..3 {
            t.s.refund_claude_run();
        }
        assert_eq!(t.s.status()["budget"]["used"], 0);
        t.s.request_claude_run();
        t.advance(17.0 * 60.0 * MIN);
        t.s.refund_claude_run();
        assert_eq!(t.s.status()["budget"]["used"], 0, "the new day starts at zero anyway");
    }

    #[tokio::test]
    async fn a_watcher_tasks_refund_reaches_the_schedulers_budget() {
        let t = quick(json!({ "watchers": { "conflicts": { "enabled": true } } }), vec![("watcher.conflicts", task_fn(|ctx: TaskContext| async move {
            assert!(ctx.request_claude_run().granted);
            ctx.refund_claude_run();
            Ok(TaskResult::ok())
        }))]);
        t.s.tick().await;
        assert_eq!(t.s.status()["budget"]["used"], 0);
    }

    #[tokio::test]
    async fn with_no_history_embed_registered_the_loop_sleeps_a_minute_at_a_time_never_a_second() {
        let t = quick(json!({}), vec![("maintenance", noop())]);
        t.advance(60.0 * MIN);
        assert_eq!(t.s.tick().await, ["maintenance"]);
        t.s.start();
        let at = t.clock.timers.lock().unwrap().iter().rev().find(|x| !x.cleared).unwrap().at;
        assert_eq!(at - t.clock.now(), MIN);
        t.s.stop();
    }

    #[tokio::test]
    async fn a_registered_history_embed_that_fails_is_retried_every_30_minutes_not_every_second() {
        let t = quick(json!({}), vec![("maintenance", noop()), ("history.embed", task_fn(|_| async { Ok(TaskResult::failed("boom")) }))]);
        t.advance(3.0 * MIN);
        assert_eq!(t.s.tick().await, ["maintenance", "history.embed"]);
        t.s.start();
        let at = t.clock.timers.lock().unwrap().iter().rev().find(|x| !x.cleared).unwrap().at;
        assert_eq!(at - t.clock.now(), MIN);
        t.s.stop();
        t.advance(10.0 * MIN);
        assert!(t.s.tick().await.is_empty());
    }

    #[test]
    fn watcher_task_names_are_the_registered_task_ids() {
        assert_eq!(watcher_task_names(), ["watcher.conflicts", "watcher.assignedBugs", "watcher.reviewRequests"]);
    }
}
