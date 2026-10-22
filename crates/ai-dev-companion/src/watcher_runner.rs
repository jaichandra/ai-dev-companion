//! The three watchers as scheduler tasks (port of `core/watcher-runner.ts`):
//! fetch what's new, filter it with the `watchers` rules, let the on-prem model
//! veto what doesn't look worth a run (when an LLM is available), ask the
//! scheduler for a Claude run, start the job and put an inbox item in. Sign-in
//! is the usual chain (a cached browser session, then a saved token); with
//! neither, the task reports "needs login". Every network and git call goes
//! through [`WatcherIo`], so tests inject all of it.

use crate::atlassian::AtlassianError;
use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::config::ConfigFn;
use crate::guard::link_guard::link_on_hosts_value;
use crate::js_text::slice_utf16;
use crate::llm::ChatMessage;
use crate::prereqs::GitRemoteRules;
use crate::route_policy::{choose_tier, Tier, TierRequest};
use crate::scheduler::{task_fn, LogFn, TaskContext, TaskFn, TaskResult};
use crate::targets::analyze_targets_from;
use crate::watchers::{self, Outcome};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MAX_PRS: usize = 20;
const MAX_EVENTS: usize = 10;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Everything a watcher reads or runs outside this process.
#[async_trait]
pub trait WatcherIo: Send + Sync {
    async fn list_dashboard_pull_requests(&self, role: &str) -> Result<Vec<Value>, AtlassianError>;
    /// `{conflicted}` of the PR's merge status.
    async fn get_merge_status(&self, project: &str, repo: &str, pr_id: i64) -> Result<Value, AtlassianError>;
    async fn search_issues(&self, jql: &str) -> Result<Vec<Value>, AtlassianError>;
    async fn who_am_i(&self) -> Option<String>;
    fn has_cached_analysis(&self, issue_key: &str) -> bool;
    /// The configured clone of PROJECT/repo, or `None`. Never infers or writes config.
    fn repo_path(&self, project: &str, repo: &str) -> Option<String>;
    async fn git_fetch(&self, repo_path: &str, branch: &str) -> Result<(), String>;
}

/// The LLM, for the triage veto.
#[async_trait]
pub trait WatcherLlm: Send + Sync {
    async fn available(&self) -> bool;
    /// The parsed JSON answer, if the model gave one.
    async fn chat(&self, system: &str, messages: &[ChatMessage], schema: &Value, max_tokens: f64) -> Result<Option<Value>, String>;
}

/// The inbox.
pub trait NotificationSink: Send + Sync {
    fn add(&self, item: &Value) -> Result<(), String>;
}

/// The service's `startFeatureJob` with `via: "watcher"` and a background context.
#[async_trait]
pub trait StartJob: Send + Sync {
    async fn start_job(&self, feature_id: &str, payload: Value, watcher: &str) -> Result<Value, String>;
}

/// Whether a job of a feature for a scope key is still open: `(scope_key, feature_id)`.
pub type ActiveJobFn = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

pub struct WatcherDeps {
    pub config: ConfigFn,
    pub io: Arc<dyn WatcherIo>,
    pub llm: Arc<dyn WatcherLlm>,
    pub notifications: Arc<dyn NotificationSink>,
    pub enabled_feature_ids: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// A job of `feature_id` for `scope_key` that isn't finished yet.
    pub active_job_for: ActiveJobFn,
    pub start_job: Arc<dyn StartJob>,
    /// The git host and issue tracker's base URLs: an inbox link may only point at these hosts.
    pub base_urls: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    pub git: GitRemoteRules,
    pub log: Option<LogFn>,
}

#[derive(Default)]
struct Stats {
    onprem: i64,
    rules: i64,
    failed: i64,
}

impl Stats {
    fn to_json(&self) -> Value {
        json!({ "onprem": self.onprem, "rules": self.rules, "failed": self.failed })
    }
}

#[derive(Default)]
struct Counts {
    started: i64,
    inboxed: i64,
    deferred: i64,
}

/// A PR from a fork lives in another repository: this clone's `origin` has no such branch.
fn is_fork(pr: &Value) -> bool {
    let from = &pr["fromRepo"];
    let (Some(pk), Some(slug)) = (from["projectKey"].as_str().filter(|s| !s.is_empty()), from["slug"].as_str().filter(|s| !s.is_empty())) else { return false };
    pk.to_lowercase() != pr["project"].as_str().unwrap_or("").to_lowercase() || slug.to_lowercase() != pr["repo"].as_str().unwrap_or("").to_lowercase()
}

fn is_login_problem(err: &AtlassianError) -> bool {
    matches!(err, AtlassianError::AuthSetup(_)) || matches!(err.status(), Some(401 | 403))
}

fn login_failure(err: &AtlassianError) -> TaskResult {
    let mut r = TaskResult::failed(&err.to_string());
    r.needs_login = is_login_problem(err);
    r
}

fn pr_id(pr: &Value) -> i64 {
    pr["id"].as_i64().unwrap_or(0)
}

/// What the real `git fetch` runs: no prompt, a timeout, one branch only.
#[derive(Clone)]
pub struct SystemGitFetch {
    pub command: String,
}

impl Default for SystemGitFetch {
    fn default() -> Self {
        Self { command: "git".into() }
    }
}

impl SystemGitFetch {
    pub async fn fetch(&self, repo_path: &str, branch: &str) -> Result<(), String> {
        if !is_safe_branch_name(branch) {
            return Err("not a safe branch name".into());
        }
        let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.args(["-C", repo_path, "fetch", "--no-tags", "origin", &refspec]).env("GIT_TERMINAL_PROMPT", "0").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| format!("git fetch failed: {e}"))?;
        match tokio::time::timeout(FETCH_TIMEOUT, child.wait_with_output()).await {
            Err(_) => Err("git fetch failed: timed out".into()),
            Ok(Err(e)) => Err(format!("git fetch failed: {e}")),
            Ok(Ok(out)) if out.status.success() => Ok(()),
            Ok(Ok(out)) => {
                let first = String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("").to_string();
                Err(format!("git fetch failed: {first}"))
            }
        }
    }
}

/// The real io: the git host and issue tracker through the usual sign-in chain, git with no prompt.
pub struct SystemWatcherIo {
    pub providers: crate::providers::Providers,
    pub config: ConfigFn,
    pub analysis_cached: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub git: SystemGitFetch,
}

#[async_trait]
impl WatcherIo for SystemWatcherIo {
    async fn list_dashboard_pull_requests(&self, role: &str) -> Result<Vec<Value>, AtlassianError> {
        self.providers.git.list_dashboard_pull_requests(&Default::default(), role).await
    }
    async fn get_merge_status(&self, project: &str, repo: &str, pr_id: i64) -> Result<Value, AtlassianError> {
        self.providers.git.get_merge_status(&Default::default(), project, repo, pr_id).await
    }
    async fn search_issues(&self, jql: &str) -> Result<Vec<Value>, AtlassianError> {
        self.providers.issues.search_issues(&Default::default(), jql, Some(20)).await
    }
    async fn who_am_i(&self) -> Option<String> {
        self.providers.git.who_am_i(&Default::default()).await.ok().flatten()
    }
    fn has_cached_analysis(&self, key: &str) -> bool {
        (self.analysis_cached)(key)
    }
    fn repo_path(&self, project: &str, repo: &str) -> Option<String> {
        let want = format!("{project}/{repo}").to_lowercase();
        let config = (self.config)();
        let dir = config["repos"].as_object()?.iter().find(|(k, _)| k.to_lowercase() == want).and_then(|(_, v)| v.as_str())?.to_string();
        Path::new(&dir).join(".git").exists().then_some(dir)
    }
    async fn git_fetch(&self, repo_path: &str, branch: &str) -> Result<(), String> {
        self.git.fetch(repo_path, branch).await
    }
}

pub struct WatcherRunner {
    deps: WatcherDeps,
}

impl WatcherRunner {
    pub fn new(deps: WatcherDeps) -> Arc<Self> {
        Arc::new(Self { deps })
    }

    /// The three tasks, named as the scheduler registers them.
    pub fn tasks(self: &Arc<Self>) -> Vec<(String, TaskFn)> {
        let (a, b, c) = (self.clone(), self.clone(), self.clone());
        vec![
            ("watcher.conflicts".into(), task_fn(move |ctx| { let r = a.clone(); async move { r.conflicts(&ctx).await } })),
            ("watcher.assignedBugs".into(), task_fn(move |ctx| { let r = b.clone(); async move { r.assigned_bugs(&ctx).await } })),
            ("watcher.reviewRequests".into(), task_fn(move |ctx| { let r = c.clone(); async move { r.review_requests(&ctx).await } })),
        ]
    }

    fn log(&self, message: &str) {
        match &self.deps.log {
            Some(f) => f(message),
            None => println!("[watchers] {message}"),
        }
    }

    /// An inbox item opens a link on click: only https on the configured git host or tracker host.
    fn with_safe_url(&self, mut event: Value) -> Value {
        let url = link_on_hosts_value(event.get("url"), &(self.deps.base_urls)());
        event["url"] = url.map_or(Value::Null, Value::String);
        event
    }

    /// The on-prem veto, when the LLM is there; `None` means "rules only".
    async fn triage(&self, event: &Value, stats: &mut Stats, onprem: bool) -> Option<(bool, String)> {
        let (system, messages) = watchers::triage_prompt(event);
        let bytes = (system.len() + messages[0].content.len()) as f64;
        if choose_tier(TierRequest { task: Some("triage"), bytes: Some(bytes), needs_tools: false }, onprem).tier != Tier::Onprem {
            stats.rules += 1;
            return None;
        }
        match self.deps.llm.chat(&system, &messages, &watchers::triage_schema(), 1024.0).await {
            Ok(value) => {
                if let Some(parsed) = value.as_ref().and_then(watchers::parse_triage) {
                    stats.onprem += 1;
                    return Some(parsed);
                }
            }
            Err(e) => self.log(&format!("WARN - on-prem triage failed, using the rules alone: {e}")),
        }
        stats.failed += 1;
        None
    }

    /// Decide, ask for a run, start it, and file the inbox item. Returns whether the event is
    /// settled (`false`: "busy", try again next tick).
    async fn act(&self, ctx: &TaskContext, event: &Value, decision: &watchers::Decision, feature_id: &str, payload: Value, counts: &mut Counts) -> bool {
        let mut outcome = Outcome::Skipped { reason: Some(decision.reason.clone()) };
        if decision.worth {
            let grant = ctx.request_claude_run();
            if !grant.granted && grant.reason == Some("busy") {
                counts.deferred += 1;
                return false;
            }
            if !grant.granted {
                outcome = Outcome::Skipped { reason: grant.reason.map(String::from) };
            } else {
                match self.deps.start_job.start_job(feature_id, payload, event["watcher"].as_str().unwrap_or("")).await {
                    Ok(job) => {
                        let id = job["id"].as_str().unwrap_or("").to_string();
                        ctx.track_background_job(&id);
                        counts.started += 1;
                        outcome = Outcome::Prewarmed { job_id: id, feature_id: feature_id.to_string() };
                    }
                    Err(e) => {
                        // The run never began: it must not cost the day's budget.
                        ctx.refund_claude_run();
                        outcome = Outcome::Skipped { reason: Some(format!("it couldn't start ({})", slice_utf16(&e, 160))) };
                    }
                }
            }
        }
        self.file(event, &outcome, Some(counts));
        true
    }

    fn file(&self, event: &Value, outcome: &Outcome, counts: Option<&mut Counts>) {
        match self.deps.notifications.add(&watchers::notification_for(event, outcome)) {
            Ok(()) => {
                if let Some(c) = counts {
                    c.inboxed += 1;
                }
            }
            Err(e) => self.log(&format!("WARN - couldn't add an inbox item: {e}")),
        }
    }

    async fn on_prem(&self, ctx: &TaskContext, events: &[Value]) -> bool {
        !events.is_empty() && !ctx.quiet && self.deps.llm.available().await
    }

    pub async fn conflicts(&self, ctx: &TaskContext) -> Result<TaskResult, String> {
        let git = self.deps.git;
        let all_prs = match self.deps.io.list_dashboard_pull_requests("AUTHOR").await {
            Ok(p) => p,
            Err(e) => return Ok(login_failure(&e)),
        };
        let prs: Vec<Value> = all_prs.iter().take(MAX_PRS).cloned().collect();
        let mut merges = Map::new();
        let mut merge_errors = 0i64;
        for pr in &prs {
            match self.deps.io.get_merge_status(pr["project"].as_str().unwrap_or(""), pr["repo"].as_str().unwrap_or(""), pr_id(pr)).await {
                Ok(m) => {
                    merges.insert(watchers::pr_key(pr, git), m);
                }
                Err(_) => merge_errors += 1,
            }
        }
        let mut seen = ctx.seen("conflicts");
        let events: Vec<Value> = watchers::conflict_events(&prs, &merges, &seen, git).into_iter().take(MAX_EVENTS).map(|e| self.with_safe_url(e)).collect();
        let onprem = self.on_prem(ctx, &events).await;
        let (mut stats, mut counts) = (Stats::default(), Counts::default());
        let enabled = (self.deps.enabled_feature_ids)().iter().any(|f| f == "resolve-conflict");
        for event in &events {
            let pr = &event["pr"];
            let (project, repo) = (pr["project"].as_str().unwrap_or(""), pr["repo"].as_str().unwrap_or(""));
            let key = event["key"].as_str().unwrap_or("");
            let rule = watchers::conflict_rule(event, enabled, self.deps.io.repo_path(project, repo).is_some(), (self.deps.active_job_for)(key, "resolve-conflict"));
            let rule = if rule.worth && is_fork(pr) { watchers::Rule { worth: false, reason: "it comes from a fork, which this clone can't fetch".into() } } else { rule };
            let triage = if rule.worth { self.triage(event, &mut stats, onprem).await } else { None };
            let decision = watchers::decide(&rule, triage.as_ref());
            let payload = json!({ "project": pr["project"], "repo": pr["repo"], "prId": pr["id"], "sourceBranch": pr["fromBranch"], "destBranch": pr["toBranch"] });
            if self.act(ctx, event, &decision, "resolve-conflict", payload, &mut counts).await {
                seen = watchers::mark_seen(&seen, key, event["stamp"].as_str().unwrap_or(""));
                // Saved per settled event: a crash mid-loop must not start the same job again.
                ctx.set_seen("conflicts", seen.clone());
            }
        }
        // Pruned against every PR the dashboard returned, not just the 20 looked at.
        let live: Vec<String> = all_prs.iter().map(|p| watchers::pr_key(p, git)).collect();
        ctx.set_seen("conflicts", watchers::prune_seen(&seen, &live.iter().map(String::as_str).collect::<Vec<_>>()));
        Ok(TaskResult::ok()
            .metric("found", json!(prs.len()))
            .metric("events", json!(events.len()))
            .metric("started", json!(counts.started))
            .metric("inboxed", json!(counts.inboxed))
            .metric("deferred", json!(counts.deferred))
            .metric("mergeErrors", json!(merge_errors))
            .metric("triage", stats.to_json())
            .metric("tier", json!(if stats.onprem > 0 { "onprem" } else { "rules" })))
    }

    pub async fn assigned_bugs(&self, ctx: &TaskContext) -> Result<TaskResult, String> {
        let targets = analyze_targets_from(&(self.deps.config)());
        let jql = watchers::assigned_bugs_jql(Some(&targets.projects), Some(&targets.issue_types))?;
        let issues = match self.deps.io.search_issues(&jql).await {
            Ok(i) => i,
            Err(e) => return Ok(login_failure(&e)),
        };
        let mut seen = ctx.seen("assignedBugs");
        let events: Vec<Value> = watchers::bug_events(&issues, &seen).into_iter().take(MAX_EVENTS).map(|e| self.with_safe_url(e)).collect();
        let onprem = self.on_prem(ctx, &events).await;
        let (mut stats, mut counts) = (Stats::default(), Counts::default());
        let enabled = (self.deps.enabled_feature_ids)().iter().any(|f| f == "analyze-issue");
        for event in &events {
            let key = event["key"].as_str().unwrap_or("");
            let rule = watchers::bug_rule(event, enabled, self.deps.io.has_cached_analysis(key), (self.deps.active_job_for)(&format!("jira:{key}"), "analyze-issue"));
            let triage = if rule.worth { self.triage(event, &mut stats, onprem).await } else { None };
            let decision = watchers::decide(&rule, triage.as_ref());
            if self.act(ctx, event, &decision, "analyze-issue", json!({ "issueKey": key }), &mut counts).await {
                seen = watchers::mark_seen(&seen, key, event["stamp"].as_str().unwrap_or(""));
                ctx.set_seen("assignedBugs", seen.clone());
            }
        }
        // (mark_seen keeps at most 500 entries; a ticket that drops out of the "updated in the last
        // day" list is not forgotten, or it would come back as new.)
        ctx.set_seen("assignedBugs", seen);
        Ok(TaskResult::ok()
            .metric("found", json!(issues.len()))
            .metric("events", json!(events.len()))
            .metric("started", json!(counts.started))
            .metric("inboxed", json!(counts.inboxed))
            .metric("deferred", json!(counts.deferred))
            .metric("triage", stats.to_json())
            .metric("tier", json!(if stats.onprem > 0 { "onprem" } else { "rules" })))
    }

    pub async fn review_requests(&self, ctx: &TaskContext) -> Result<TaskResult, String> {
        let git = self.deps.git;
        let all_prs = match self.deps.io.list_dashboard_pull_requests("REVIEWER").await {
            Ok(p) => p,
            Err(e) => return Ok(login_failure(&e)),
        };
        let prs: Vec<Value> = all_prs.iter().take(MAX_PRS).cloned().collect();
        let me = self.deps.io.who_am_i().await;
        let mut seen = ctx.seen("reviewRequests");
        let events: Vec<Value> = watchers::review_events(&prs, me.as_deref(), &seen, git).into_iter().take(MAX_EVENTS).map(|e| self.with_safe_url(e)).collect();
        let mut fetched = 0i64;
        for event in &events {
            let pr = &event["pr"];
            let repo_path = self.deps.io.repo_path(pr["project"].as_str().unwrap_or(""), pr["repo"].as_str().unwrap_or(""));
            let branch = pr["fromBranch"].as_str().unwrap_or("");
            let fail = |r: &str| Outcome::FetchFailed { reason: Some(r.to_string()) };
            let outcome = match repo_path {
                None => fail("no local clone is set up"),
                Some(_) if is_fork(pr) => fail("it comes from a fork, which this clone can't fetch"),
                Some(_) if branch.is_empty() || !is_safe_branch_name(branch) => fail("its branch name can't be fetched safely"),
                Some(dir) => match self.deps.io.git_fetch(&dir, branch).await {
                    Ok(()) => {
                        fetched += 1;
                        Outcome::Fetched
                    }
                    Err(e) => fail(&slice_utf16(&e, 160)),
                },
            };
            self.file(event, &outcome, None);
            seen = watchers::mark_seen(&seen, event["key"].as_str().unwrap_or(""), event["stamp"].as_str().unwrap_or(""));
            ctx.set_seen("reviewRequests", seen.clone());
        }
        let live: Vec<String> = all_prs.iter().map(|p| watchers::pr_key(p, git)).collect();
        ctx.set_seen("reviewRequests", watchers::prune_seen(&seen, &live.iter().map(String::as_str).collect::<Vec<_>>()));
        Ok(TaskResult::ok().metric("found", json!(prs.len())).metric("events", json!(events.len())).metric("fetched", json!(fetched)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::scheduler::{ClaudeRunGrant, TaskHost};
    use crate::schedule::BudgetUsage;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    const KEY: &str = "bitbucket:CI/sample-app#12";
    const JOB_ID: &str = "3f2b8c1e-1111-4222-8333-444455556666";

    fn pr() -> Value {
        json!({
            "id": 12, "title": "PROJ-7: fix login", "state": "OPEN", "project": "CI", "repo": "sample-app",
            "fromBranch": "bugfix/PROJ-7", "fromSha": "aaa111", "toBranch": "master", "toSha": "bbb222",
            "url": "https://bb.example/projects/CI/repos/sample-app/pull-requests/12",
            "approvals": 0, "reviewers": [{ "name": "me", "status": "UNAPPROVED" }],
        })
    }

    fn usage() -> BudgetUsage {
        BudgetUsage { day: "2026-09-29".into(), used: 1, limit: 5, remaining: 4 }
    }

    fn pr_n(n: i64, over: Value) -> Value {
        let mut p = pr();
        p["id"] = json!(n);
        p["url"] = json!(format!("https://bb.example/projects/CI/repos/sample-app/pull-requests/{n}"));
        p["fromBranch"] = json!(format!("b{n}"));
        for (k, v) in over.as_object().into_iter().flatten() {
            p[k] = v.clone();
        }
        p
    }

    fn key_n(n: i64) -> String {
        format!("bitbucket:CI/sample-app#{n}")
    }

    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    struct FakeIo {
        calls: Calls,
        prs: Vec<Value>,
        list_error: Option<fn() -> AtlassianError>,
        search_error: Option<fn() -> AtlassianError>,
        merge: Box<dyn Fn(i64) -> Result<Value, AtlassianError> + Send + Sync>,
        cached: bool,
        has_clone: bool,
    }

    impl FakeIo {
        fn new(calls: Calls) -> Self {
            Self { calls, prs: vec![pr()], list_error: None, search_error: None, merge: Box::new(|_| Ok(json!({ "conflicted": true }))), cached: false, has_clone: true }
        }
        fn note(&self, parts: &[&str]) {
            self.calls.lock().unwrap().push(parts.iter().map(|s| s.to_string()).collect());
        }
    }

    #[async_trait]
    impl WatcherIo for FakeIo {
        async fn list_dashboard_pull_requests(&self, role: &str) -> Result<Vec<Value>, AtlassianError> {
            self.note(&["dashboard", role]);
            self.list_error.map_or(Ok(self.prs.clone()), |e| Err(e()))
        }
        async fn get_merge_status(&self, p: &str, r: &str, id: i64) -> Result<Value, AtlassianError> {
            self.note(&["merge", p, r, &id.to_string()]);
            (self.merge)(id)
        }
        async fn search_issues(&self, jql: &str) -> Result<Vec<Value>, AtlassianError> {
            self.note(&["search", jql]);
            self.search_error.map_or(Ok(vec![json!({ "key": "PROJ-7", "summary": "Login broken", "status": "Open", "priority": "High", "url": "https://jira.example/browse/PROJ-7" })]), |e| Err(e()))
        }
        async fn who_am_i(&self) -> Option<String> {
            Some("me".into())
        }
        fn has_cached_analysis(&self, _: &str) -> bool {
            self.cached
        }
        fn repo_path(&self, p: &str, r: &str) -> Option<String> {
            (self.has_clone && format!("{p}/{r}") == "CI/sample-app").then(|| "/clones/sample-app".to_string())
        }
        async fn git_fetch(&self, dir: &str, branch: &str) -> Result<(), String> {
            self.note(&["fetch", dir, branch]);
            Ok(())
        }
    }

    struct FakeLlm {
        available: bool,
        reply: Result<Option<Value>, String>,
        prompts: Arc<Mutex<Vec<(Value, String)>>>,
        asked_available: Arc<Mutex<i32>>,
    }

    #[async_trait]
    impl WatcherLlm for FakeLlm {
        async fn available(&self) -> bool {
            *self.asked_available.lock().unwrap() += 1;
            self.available
        }
        async fn chat(&self, _: &str, messages: &[ChatMessage], schema: &Value, _: f64) -> Result<Option<Value>, String> {
            self.prompts.lock().unwrap().push((schema.clone(), messages[0].content.clone()));
            self.reply.clone()
        }
    }

    struct Inbox(Arc<Mutex<Vec<Value>>>);
    impl NotificationSink for Inbox {
        fn add(&self, item: &Value) -> Result<(), String> {
            self.0.lock().unwrap().push(item.clone());
            Ok(())
        }
    }

    struct Starter {
        calls: Calls,
        fail: Option<String>,
    }

    #[async_trait]
    impl StartJob for Starter {
        async fn start_job(&self, feature_id: &str, payload: Value, watcher: &str) -> Result<Value, String> {
            self.calls.lock().unwrap().push(vec!["start".into(), feature_id.into(), payload.to_string(), watcher.into()]);
            match &self.fail {
                Some(m) => Err(m.clone()),
                None => Ok(json!({ "id": JOB_ID })),
            }
        }
    }

    struct Host {
        calls: Calls,
        grants: Mutex<VecDeque<ClaudeRunGrant>>,
        tracked: Mutex<Vec<String>>,
        seen_store: Mutex<Map<String, Value>>,
        saves: Mutex<Vec<(String, usize)>>,
    }

    impl TaskHost for Host {
        fn now(&self) -> f64 {
            0.0
        }
        fn request_claude_run(&self) -> ClaudeRunGrant {
            self.calls.lock().unwrap().push(vec!["grant".into()]);
            self.grants.lock().unwrap().pop_front().unwrap_or(ClaudeRunGrant { granted: true, reason: None, usage: usage() })
        }
        fn refund_claude_run(&self) {
            self.calls.lock().unwrap().push(vec!["refund".into()]);
        }
        fn track_background_job(&self, id: &str) {
            self.tracked.lock().unwrap().push(id.into());
        }
        fn seen(&self, w: &str) -> Map<String, Value> {
            self.seen_store.lock().unwrap().get(w).and_then(Value::as_object).cloned().unwrap_or_default()
        }
        fn set_seen(&self, w: &str, s: Map<String, Value>) {
            self.saves.lock().unwrap().push((w.into(), s.len()));
            self.seen_store.lock().unwrap().insert(w.into(), Value::Object(s));
        }
    }

    struct H {
        runner: Arc<WatcherRunner>,
        ctx: TaskContext,
        calls: Calls,
        inbox: Arc<Mutex<Vec<Value>>>,
        host: Arc<Host>,
        prompts: Arc<Mutex<Vec<(Value, String)>>>,
        asked_available: Arc<Mutex<i32>>,
    }

    type IoTweak = Box<dyn FnOnce(&mut FakeIo)>;

    struct Opts {
        io: Option<IoTweak>,
        llm_available: bool,
        llm_reply: Result<Option<Value>, String>,
        grants: Vec<ClaudeRunGrant>,
        enabled: Vec<&'static str>,
        seen: Value,
        start_error: Option<String>,
        quiet: bool,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self { io: None, llm_available: false, llm_reply: Err("no chat expected".into()), grants: vec![], enabled: vec!["resolve-conflict", "analyze-issue"], seen: json!({}), start_error: None, quiet: false }
        }
    }

    fn harness(o: Opts) -> H {
        let calls: Calls = Arc::new(Mutex::new(vec![]));
        let inbox = Arc::new(Mutex::new(vec![]));
        let prompts = Arc::new(Mutex::new(vec![]));
        let asked_available = Arc::new(Mutex::new(0));
        let mut io = FakeIo::new(calls.clone());
        if let Some(f) = o.io {
            f(&mut io);
        }
        let config = json!({ "repos": { "CI/sample-app": "/clones/sample-app" }, "analyzeIssue": { "projects": ["PROJ"], "issueTypes": ["Bug"] }, "bitbucket": { "baseUrl": "https://bb.example" }, "jira": { "baseUrl": "https://jira.example" } });
        let enabled: Vec<String> = o.enabled.iter().map(|s| s.to_string()).collect();
        let deps = WatcherDeps {
            config: Arc::new(move || config.clone()),
            io: Arc::new(io),
            llm: Arc::new(FakeLlm { available: o.llm_available, reply: o.llm_reply, prompts: prompts.clone(), asked_available: asked_available.clone() }),
            notifications: Arc::new(Inbox(inbox.clone())),
            enabled_feature_ids: Arc::new(move || enabled.clone()),
            active_job_for: Arc::new(|_, _| false),
            start_job: Arc::new(Starter { calls: calls.clone(), fail: o.start_error }),
            base_urls: Arc::new(|| vec!["https://bb.example".into(), "https://jira.example".into()]),
            git: GitRemoteRules::BitbucketDc,
            log: Some(Arc::new(|_| {})),
        };
        let mut seen_store = Map::new();
        for w in ["conflicts", "assignedBugs", "reviewRequests"] {
            seen_store.insert(w.into(), json!({}));
        }
        for (k, v) in o.seen.as_object().unwrap() {
            seen_store.insert(k.clone(), v.clone());
        }
        let host = Arc::new(Host { calls: calls.clone(), grants: Mutex::new(o.grants.into()), tracked: Mutex::new(vec![]), seen_store: Mutex::new(seen_store), saves: Mutex::new(vec![]) });
        H { runner: WatcherRunner::new(deps), ctx: TaskContext::new(host.clone(), o.quiet), calls, inbox, host, prompts, asked_available }
    }

    impl H {
        fn calls_of(&self, kind: &str) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().iter().filter(|c| c[0] == kind).cloned().collect()
        }
        fn has(&self, kinds: &[&str]) -> bool {
            self.calls.lock().unwrap().iter().any(|c| kinds.contains(&c[0].as_str()))
        }
        fn seen(&self, w: &str) -> Value {
            self.host.seen_store.lock().unwrap()[w].clone()
        }
        fn inbox(&self) -> Vec<Value> {
            self.inbox.lock().unwrap().clone()
        }
    }

    fn with_io(f: impl FnOnce(&mut FakeIo) + 'static) -> Opts {
        Opts { io: Some(Box::new(f)), ..Default::default() }
    }

    fn metric(r: &TaskResult, k: &str) -> Value {
        r.metrics[k].clone()
    }

    #[tokio::test]
    async fn conflicts_a_conflicted_pr_with_a_clone_starts_resolve_conflict_rules_only_tracked_and_in_the_inbox() {
        let h = harness(Opts::default());
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        let payload = json!({ "project": "CI", "repo": "sample-app", "prId": 12, "sourceBranch": "bugfix/PROJ-7", "destBranch": "master" });
        assert_eq!(h.calls_of("start"), [vec!["start".to_string(), "resolve-conflict".into(), payload.to_string(), "conflicts".into()]]);
        assert_eq!(*h.host.tracked.lock().unwrap(), [JOB_ID]);
        assert_eq!(h.inbox().len(), 1);
        assert_eq!((h.inbox()[0]["jobId"].clone(), h.inbox()[0]["kind"].clone()), (json!(JOB_ID), json!("conflict")));
        assert_eq!(h.seen("conflicts"), json!({ KEY: "aaa111:bbb222" }));
        assert_eq!(report.metrics, json!({ "found": 1, "events": 1, "started": 1, "inboxed": 1, "deferred": 0, "mergeErrors": 0, "triage": { "onprem": 0, "rules": 1, "failed": 0 }, "tier": "rules" }).as_object().unwrap().clone());
        assert_eq!(metric(&h.runner.conflicts(&h.ctx).await.unwrap(), "events"), 0, "seen at these commits");
    }

    #[tokio::test]
    async fn conflicts_the_on_prem_triage_can_veto_a_run_and_sees_the_pr_only_as_fenced_data() {
        let h = harness(Opts { llm_available: true, llm_reply: Ok(Some(json!({ "worth": false, "reason": "only a version bump" }))), ..Default::default() });
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        assert!(!h.has(&["grant", "start"]));
        assert!(h.inbox()[0]["body"].as_str().unwrap().contains("Not pre-warmed: only a version bump"));
        let (schema, content) = h.prompts.lock().unwrap()[0].clone();
        assert_eq!(schema["required"], json!(["worth", "reason"]));
        assert!(content.starts_with("<untrusted-event>"));
        assert_eq!((metric(&report, "tier"), metric(&report, "triage")), (json!("onprem"), json!({ "onprem": 1, "rules": 0, "failed": 0 })));
    }

    #[tokio::test]
    async fn conflicts_a_broken_triage_reply_falls_back_to_the_rules() {
        let h = harness(Opts { llm_available: true, llm_reply: Ok(Some(json!({ "nope": 1 }))), ..Default::default() });
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        assert_eq!((metric(&report, "triage"), metric(&report, "started")), (json!({ "onprem": 0, "rules": 0, "failed": 1 }), json!(1)));
    }

    #[tokio::test]
    async fn conflicts_no_budget_files_an_inbox_item_busy_leaves_the_event_for_the_next_tick() {
        let spent = harness(Opts { grants: vec![ClaudeRunGrant { granted: false, reason: Some("budget"), usage: usage() }], ..Default::default() });
        spent.runner.conflicts(&spent.ctx).await.unwrap();
        assert!(!spent.has(&["start"]));
        assert!(spent.inbox()[0]["body"].as_str().unwrap().contains("budget is used up"));
        assert_eq!(spent.seen("conflicts"), json!({ KEY: "aaa111:bbb222" }));

        let busy = harness(Opts { grants: vec![ClaudeRunGrant { granted: false, reason: Some("busy"), usage: usage() }], ..Default::default() });
        let report = busy.runner.conflicts(&busy.ctx).await.unwrap();
        assert_eq!(metric(&report, "deferred"), 1);
        assert!(busy.inbox().is_empty());
        assert_eq!(busy.seen("conflicts"), json!({}), "not seen, so it's tried again");
    }

    #[tokio::test]
    async fn conflicts_no_clone_or_the_feature_off_means_an_inbox_item_and_no_claude_run() {
        let no_clone = harness(with_io(|io| io.has_clone = false));
        no_clone.runner.conflicts(&no_clone.ctx).await.unwrap();
        assert!(!no_clone.has(&["grant"]));
        assert!(no_clone.inbox()[0]["body"].as_str().unwrap().contains("no local clone of CI/sample-app"));
        let off = harness(Opts { enabled: vec!["analyze-issue"], ..Default::default() });
        off.runner.conflicts(&off.ctx).await.unwrap();
        assert!(off.inbox()[0]["body"].as_str().unwrap().contains("turned off"));
    }

    #[tokio::test]
    async fn no_sign_in_anywhere_the_task_fails_with_needs_login() {
        let no_auth = harness(with_io(|io| io.list_error = Some(|| AtlassianError::AuthSetup("Bitbucket: no session and no token.".into()))));
        let r = no_auth.runner.conflicts(&no_auth.ctx).await.unwrap();
        assert_eq!((r.outcome, r.needs_login, r.error.as_deref()), (crate::scheduler::Outcome::Failed, true, Some("Bitbucket: no session and no token.")));
        let denied = harness(with_io(|io| io.search_error = Some(|| AtlassianError::Status { message: "HTTP 401".into(), status: 401 })));
        assert!(denied.runner.assigned_bugs(&denied.ctx).await.unwrap().needs_login);
        let down = harness(with_io(|io| io.list_error = Some(|| AtlassianError::Status { message: "HTTP 503".into(), status: 503 })));
        assert!(!down.runner.review_requests(&down.ctx).await.unwrap().needs_login);
    }

    #[tokio::test]
    async fn assigned_bugs_searches_the_configured_projects_analyzes_a_new_ticket_once_skips_an_analyzed_one() {
        let h = harness(Opts::default());
        h.runner.assigned_bugs(&h.ctx).await.unwrap();
        assert!(h.calls_of("search")[0][1].contains("project in (\"PROJ\") AND issuetype in (\"Bug\")"), "{:?}", h.calls_of("search"));
        assert_eq!(h.calls_of("start")[0][1..], ["analyze-issue".to_string(), json!({ "issueKey": "PROJ-7" }).to_string(), "assignedBugs".to_string()]);
        assert_eq!(h.inbox()[0]["url"], "https://jira.example/browse/PROJ-7");
        assert_eq!(h.seen("assignedBugs"), json!({ "PROJ-7": "analyzed" }));
        let cached = harness(with_io(|io| io.cached = true));
        cached.runner.assigned_bugs(&cached.ctx).await.unwrap();
        assert!(!cached.has(&["grant"]));
        assert!(cached.inbox()[0]["body"].as_str().unwrap().contains("already analyzed"));
    }

    #[tokio::test]
    async fn review_requests_fetches_the_branch_into_the_clone_never_asks_for_a_claude_run() {
        let h = harness(Opts::default());
        let report = h.runner.review_requests(&h.ctx).await.unwrap();
        assert_eq!(h.calls_of("fetch"), [vec!["fetch".to_string(), "/clones/sample-app".into(), "bugfix/PROJ-7".into()]]);
        assert!(!h.has(&["grant", "start"]));
        assert_eq!(h.inbox()[0]["kind"], "review-request");
        assert!(h.inbox()[0]["body"].as_str().unwrap().contains("branch is fetched"));
        assert_eq!(report.metrics, json!({ "found": 1, "events": 1, "fetched": 1 }).as_object().unwrap().clone());

        let unsafe_branch = harness(with_io(|io| io.prs = vec![pr_n(12, json!({ "fromBranch": "-x" }))]));
        unsafe_branch.runner.review_requests(&unsafe_branch.ctx).await.unwrap();
        assert!(!unsafe_branch.has(&["fetch"]));
        assert!(unsafe_branch.inbox()[0]["body"].as_str().unwrap().contains("can't be fetched safely"));
    }

    #[tokio::test]
    async fn conflicts_only_the_author_dashboard_is_read_never_the_reviewer_one() {
        let h = harness(Opts::default());
        h.runner.conflicts(&h.ctx).await.unwrap();
        assert_eq!(h.calls_of("dashboard"), [vec!["dashboard".to_string(), "AUTHOR".into()]]);
        let r = harness(Opts::default());
        r.runner.review_requests(&r.ctx).await.unwrap();
        assert_eq!(r.calls_of("dashboard"), [vec!["dashboard".to_string(), "REVIEWER".into()]]);
    }

    #[tokio::test]
    async fn triage_is_never_asked_when_the_rules_already_say_no_so_it_can_only_veto() {
        let h = harness(Opts { io: Some(Box::new(|io| io.has_clone = false)), llm_available: true, llm_reply: Ok(Some(json!({ "worth": true, "reason": "yes" }))), ..Default::default() });
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        assert!(h.prompts.lock().unwrap().is_empty());
        assert_eq!(metric(&report, "started"), 0);
        assert!(!h.has(&["grant", "start"]));
    }

    #[tokio::test]
    async fn re_running_the_same_commits_yields_the_same_inbox_key_which_carries_the_commit_stamp() {
        let h = harness(Opts::default());
        h.runner.conflicts(&h.ctx).await.unwrap();
        h.host.seen_store.lock().unwrap().insert("conflicts".into(), json!({}));
        h.runner.conflicts(&h.ctx).await.unwrap();
        let keys: std::collections::HashSet<String> = h.inbox().iter().map(|i| i["key"].as_str().unwrap().to_string()).collect();
        assert_eq!(keys.len(), 1, "same commits, same key");
        assert!(keys.iter().next().unwrap().contains("aaa111:bbb222"));
    }

    #[tokio::test]
    async fn the_real_git_fetch_refuses_an_unsafe_branch_name_before_spawning_anything() {
        let g = SystemGitFetch { command: "/no/such/git".into() };
        assert_eq!(g.fetch("/nonexistent", "-x").await.unwrap_err(), "not a safe branch name");
        assert_eq!(g.fetch("/nonexistent", "a b").await.unwrap_err(), "not a safe branch name");
    }

    #[tokio::test]
    async fn seen_is_saved_after_each_settled_event_so_a_crash_mid_loop_cannot_start_the_same_job_twice() {
        let h = harness(with_io(|io| io.prs = vec![pr_n(1, json!({})), pr_n(2, json!({})), pr_n(3, json!({}))]));
        h.runner.conflicts(&h.ctx).await.unwrap();
        let sizes: Vec<usize> = h.host.saves.lock().unwrap().iter().map(|(_, n)| *n).collect();
        assert_eq!(sizes[..3], [1, 2, 3], "one save per settled event, before the final prune");
        let busy = harness(Opts { io: Some(Box::new(|io| io.prs = vec![pr_n(1, json!({}))])), grants: vec![ClaudeRunGrant { granted: false, reason: Some("busy"), usage: usage() }], ..Default::default() });
        busy.runner.conflicts(&busy.ctx).await.unwrap();
        assert!(busy.host.saves.lock().unwrap().iter().all(|(_, n)| *n == 0), "a deferred event is not marked seen");
    }

    #[tokio::test]
    async fn inbox_links_must_be_https_on_the_configured_host_anything_else_is_dropped() {
        let h = harness(with_io(|io| io.prs = vec![pr_n(1, json!({ "url": "http://bb.example/x" })), pr_n(2, json!({ "url": "https://evil.example/x" })), pr_n(3, json!({ "url": "javascript:alert(1)" })), pr_n(4, json!({}))]));
        h.runner.conflicts(&h.ctx).await.unwrap();
        let by_pr: std::collections::HashMap<String, Value> = h.inbox().iter().map(|i| (i["scopeKey"].as_str().unwrap().to_string(), i["url"].clone())).collect();
        assert_eq!(by_pr[&key_n(1)], Value::Null);
        assert_eq!(by_pr[&key_n(2)], Value::Null);
        assert_eq!(by_pr[&key_n(3)], Value::Null);
        assert_eq!(by_pr[&key_n(4)], "https://bb.example/projects/CI/repos/sample-app/pull-requests/4");
    }

    #[tokio::test]
    async fn the_reported_tier_is_the_one_that_actually_decided() {
        let failing = harness(Opts { llm_available: true, llm_reply: Err("llm down".into()), ..Default::default() });
        assert_eq!(metric(&failing.runner.conflicts(&failing.ctx).await.unwrap(), "tier"), "rules");
        let quiet = harness(Opts { llm_available: true, quiet: true, ..Default::default() });
        assert_eq!(metric(&quiet.runner.conflicts(&quiet.ctx).await.unwrap(), "tier"), "rules");
        assert_eq!(*quiet.asked_available.lock().unwrap(), 0, "quiet hours never ask the model");
    }

    #[tokio::test]
    async fn a_pr_from_a_fork_is_not_started_or_fetched_this_clone_has_no_such_branch() {
        let fork = || vec![pr_n(1, json!({ "fromRepo": { "projectKey": "~ME", "slug": "sample-app" } }))];
        let h = harness(with_io(move |io| io.prs = fork()));
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        assert_eq!(metric(&report, "started"), 0);
        assert!(!h.has(&["grant", "start"]));
        assert!(h.inbox()[0]["body"].as_str().unwrap().contains("fork"));
        let r = harness(with_io(move |io| io.prs = fork()));
        r.runner.review_requests(&r.ctx).await.unwrap();
        assert!(!r.has(&["fetch"]));
        assert!(r.inbox()[0]["body"].as_str().unwrap().contains("fork"));
        let s = harness(with_io(|io| io.prs = vec![pr_n(1, json!({ "fromRepo": { "projectKey": "ci", "slug": "Sample-App" } }))]));
        assert_eq!(metric(&s.runner.conflicts(&s.ctx).await.unwrap(), "started"), 1, "same repo, any case");
    }

    #[tokio::test]
    async fn caps_at_most_20_prs_looked_at_and_10_events_handled_seen_keeps_every_pr_the_dashboard_returned() {
        let many: Vec<Value> = (1..=25).map(|n| pr_n(n, json!({}))).collect();
        let h = harness(Opts { io: Some(Box::new(move |io| io.prs = many)), seen: json!({ "conflicts": { key_n(25): "old:old", key_n(99): "gone:gone" } }), ..Default::default() });
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        assert_eq!((metric(&report, "found"), metric(&report, "events")), (json!(20), json!(10)));
        assert!(h.seen("conflicts").get(key_n(25)).is_some(), "PR 25 is still open, just past the 20 looked at");
        assert!(h.seen("conflicts").get(key_n(99)).is_none(), "a PR that is gone is forgotten");
    }

    #[tokio::test]
    async fn merge_status_errors_are_counted_and_skipped_a_failed_start_is_filed_and_settled() {
        let h = harness(with_io(|io| {
            io.prs = vec![pr_n(1, json!({})), pr_n(2, json!({}))];
            io.merge = Box::new(|id| if id == 1 { Err(AtlassianError::Status { message: "HTTP 500".into(), status: 500 }) } else { Ok(json!({ "conflicted": true })) });
        }));
        let report = h.runner.conflicts(&h.ctx).await.unwrap();
        assert_eq!((metric(&report, "mergeErrors"), metric(&report, "events")), (json!(1), json!(1)));

        let failing = harness(Opts { start_error: Some("Resolve Conflict needs a Claude session".into()), ..Default::default() });
        let r = failing.runner.conflicts(&failing.ctx).await.unwrap();
        assert_eq!((metric(&r, "started"), metric(&r, "inboxed")), (json!(0), json!(1)));
        assert!(failing.inbox()[0]["body"].as_str().unwrap().contains("couldn't start (Resolve Conflict needs a Claude session)"));
        assert!(failing.seen("conflicts").get(KEY).is_some(), "settled: not retried at these commits");
    }

    #[tokio::test]
    async fn the_real_git_fetch_runs_exactly_git_c_clone_fetch_no_tags_origin_refspec_without_prompting() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("argv.txt");
        let script = d.path().join("git");
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$GIT_TERMINAL_PROMPT\" \"$@\" > '{}'\n", log.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        SystemGitFetch { command: script.to_string_lossy().into_owned() }.fetch("/clones/sample-app", "feature/a-b").await.unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim().lines().collect::<Vec<_>>(), ["0", "-C", "/clones/sample-app", "fetch", "--no-tags", "origin", "+refs/heads/feature/a-b:refs/remotes/origin/feature/a-b"]);
        let failing = d.path().join("git-fails");
        std::fs::write(&failing, "#!/bin/sh\necho 'fatal: no such remote' >&2\nexit 128\n").unwrap();
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(SystemGitFetch { command: failing.to_string_lossy().into_owned() }.fetch("/c", "main").await.unwrap_err(), "git fetch failed: fatal: no such remote");
    }

    #[tokio::test]
    async fn a_start_that_throws_gives_its_run_back_to_the_budget_a_start_that_works_does_not() {
        let failing = harness(Opts { start_error: Some("no session".into()), ..Default::default() });
        failing.runner.conflicts(&failing.ctx).await.unwrap();
        let kinds: Vec<String> = failing.calls.lock().unwrap().iter().filter(|c| c[0] == "grant" || c[0] == "refund").map(|c| c[0].clone()).collect();
        assert_eq!(kinds, ["grant", "refund"]);
        let ok = harness(Opts::default());
        ok.runner.conflicts(&ok.ctx).await.unwrap();
        assert!(!ok.has(&["refund"]));
    }

    #[test]
    fn the_runner_registers_one_task_per_watcher_under_the_schedulers_names() {
        let h = harness(Opts::default());
        let names: Vec<String> = h.runner.tasks().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, crate::scheduler::watcher_task_names());
    }
}
