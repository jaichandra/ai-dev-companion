//! The service's startup (port of the second half of `server.ts`): loads the config, migrates the
//! state folder and the saved tokens, restores jobs, opens the history, builds the enabled
//! features from the packs, wires the background work, the HTTP hooks and `/mcp`, then serves on
//! 127.0.0.1 until a signal.
//!
//! A distribution's `main` initializes the profile (`environment::init`), builds its
//! `PackRegistry<FeatureFactory>` (`packs::builtin::pack` plus its own pack) and calls
//! [`run_service`]. Tests call [`build`] with [`Overrides`] (fake providers and Claude, a temp home,
//! port 0) and drive the router directly.

use crate::atlassian::{AuthContext, Client};
use crate::auth_context::{self, AuthHub, SharedConfig};
use crate::background_wiring::{create_background_wiring, BackgroundWiringDeps, StartFeatureJob, StartVia};
use crate::claude::{ClaudeEnv, ClaudeRunner, SystemClaude};
use crate::config::{ConfigFn, ConfigStore};
use crate::credentials::{Credentials, TokenLookup};
use crate::external_tokens::ExternalTokens;
use crate::feature_registry::FeatureRegistry;
use crate::features::digest::index::ProviderDigestIo;
use crate::history_db::HistoryDb;
use crate::hooks_config::ConfigHooks;
use crate::history_record::{HistoryRules, Recorder};
use crate::http::ReqwestTransport;
use crate::integrity_baselines::BaselineStore;
use crate::jobs::{Feature, FeatureContext, JobStore, PersistenceOptions};
use crate::llm::{LlmBackend, LlmContext, LlmHandle, NoLlm};
use crate::mcp::{self, McpDeps, McpRequest};
use crate::notifications::NotificationStore;
use crate::packs::builtin::{FeatureFactory, FeatureServices};
use crate::packs::PackRegistry;
use crate::prereqs::{self, git_remote_rules};
use crate::providers::{create_providers, Deps, Providers};
use crate::repo_choice::RepoChoices;
use crate::scheduler::TaskFn;
use crate::server::{self, ApiError, AppHooks, AppState, HookResult, McpHandler as HttpMcpHandler};
use crate::site_tokens::SiteTokens;
use crate::terminal::Terminal;
use crate::work_units::WorkUnits;
use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const CHECK_OK_TTL: Duration = Duration::from_secs(5 * 60);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(2000);

/// What a test (or a dev build) swaps for the real thing.
#[derive(Default)]
pub struct Overrides {
    pub providers: Option<Providers>,
    /// Replaces the pack's LLM (tests).
    pub llm: Option<Arc<dyn LlmBackend>>,
    pub claude: Option<Arc<dyn ClaudeRunner>>,
    /// Whether this copy is the installed one (job persistence, history and state-dir migration are
    /// for the installed copy only). Default: asks the updater.
    pub stable_install: Option<bool>,
    /// Overrides `config.port`.
    pub port: Option<u16>,
    /// Skips the startup prerequisite checks (they run real commands).
    pub skip_startup_checks: bool,
}

/// Everything a distribution hands the service.
pub struct Distribution {
    pub packs: Arc<PackRegistry<FeatureFactory>>,
    pub home: PathBuf,
    pub config_path: PathBuf,
    /// The installed version, when the distribution knows it.
    pub version: Option<String>,
    /// Handlers for MCP tools the distribution's own features own (keyed by tool name).
    pub extra_mcp_handlers: HashMap<String, mcp::McpHandler>,
    /// How to start the detached update runner (the distribution's binary and its subcommand);
    /// `None`: this build can't apply an update.
    pub update_runner: Option<crate::updater::RunnerCommand>,
    pub overrides: Overrides,
}

/// Whether the service runs from the installed copy (`~/<app slug>`): the folder holding its
/// config is `<install dir>/companion-service`, so it is that folder's parent that is compared
/// with the install dir. Only the installed copy keeps history and jobs on disk and can update.
fn is_installed_copy(service_dir: &Path, home: &Path) -> bool {
    crate::updater::cannot_apply_reason(&service_dir.join(".."), home).is_none()
}

/// A built service: the router and what keeps it alive.
pub struct Running {
    pub router: Router,
    pub state: Arc<AppState>,
    pub port: u16,
    shutdown: Arc<Shutdown>,
}

struct Shutdown {
    store: Arc<JobStore>,
    recorder: Option<Arc<Recorder>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    stop_background: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl Running {
    /// Freezes job persistence and history first, so the jobs these kills fail are never saved as
    /// "failed: aborted" (their files keep "running" and the next start reconciles them), then stops
    /// the scheduler and the features' background work and kills detached children.
    pub async fn shutdown(&self) {
        let s = &self.shutdown;
        s.shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
        s.store.freeze_persistence();
        if let Some(r) = &s.recorder {
            r.freeze();
        }
        s.scheduler.stop();
        for stop in s.stop_background.lock().unwrap().drain(..) {
            stop();
        }
        crate::exec::kill_detached_children(libc::SIGTERM);
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        while crate::exec::detached_child_count() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Starts the schedulers and the features' background work (the installed copy only).
    pub fn start_background(&self, persist_jobs: bool, features: &[Arc<dyn Feature>]) {
        self.shutdown.scheduler.start();
        if persist_jobs {
            for f in features {
                if let Some(stop) = f.start_background() {
                    self.shutdown.stop_background.lock().unwrap().push(stop);
                }
            }
        }
    }
}

fn warn(msg: &str) {
    println!("[startup] WARN - {msg}");
}

fn ok(msg: &str) {
    println!("[startup] OK - {msg}");
}

pub(crate) struct CredentialTokens(pub(crate) Arc<Credentials>);

impl TokenLookup for CredentialTokens {
    fn get_token(&self, name: &str) -> Option<String> {
        self.0.get_token(name)
    }
}

/// The checks of a feature that last passed less than five minutes ago aren't re-run.
struct Readiness {
    registry: Arc<FeatureRegistry>,
    passed: Mutex<HashMap<String, Instant>>,
}

impl Readiness {
    fn assert(&self, feature_id: &str, label: &str) -> Result<(), ApiError> {
        for name in self.registry.required_checks_for(&[feature_id]) {
            if self.passed.lock().unwrap().get(&name).is_some_and(|t| t.elapsed() < CHECK_OK_TTL) {
                continue;
            }
            if let Some(check) = prereqs::run_check_by_name(&name) {
                if !check.ok {
                    return Err(ApiError::new(400, &format!("{label} can't run on this machine yet: {}", check.message)));
                }
            }
            self.passed.lock().unwrap().insert(name, Instant::now());
        }
        Ok(())
    }
}

/// The real answers behind the HTTP routes the extension calls.
struct ServiceHooks {
    cfg: Arc<ConfigHooks>,
    config_store: Arc<ConfigStore>,
    config: ConfigFn,
    home: PathBuf,
    packs: Arc<PackRegistry<FeatureFactory>>,
    registry: Arc<FeatureRegistry>,
    readiness: Readiness,
    hub: Arc<AuthHub>,
    cache_root: PathBuf,
    history: Option<Arc<HistoryDb>>,
    version: Option<String>,
    started_at: String,
    under_launchd: bool,
    service_dir: PathBuf,
    checker: Arc<crate::updater::UpdateChecker>,
}

#[async_trait]
impl AppHooks for ServiceHooks {
    fn shared_secret(&self) -> String {
        (self.config)()["sharedSecret"].as_str().unwrap_or("").to_string()
    }

    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    fn assert_feature_ready(&self, feature_id: &str, label: &str) -> Result<(), ApiError> {
        self.readiness.assert(feature_id, label)
    }

    /// Resolves the PR's clone before a repo-based feature starts, so a missing clone surfaces as
    /// the 409 `repo-not-found` the extension answers by offering to pick or clone it.
    fn assert_repo_available(&self, feature_id: &str, payload: &Value) -> Result<(), ApiError> {
        let (Some(project), Some(repo)) = (payload["project"].as_str(), payload["repo"].as_str()) else { return Ok(()) };
        if !self.registry.needs_repos(&[feature_id]) {
            return Ok(());
        }
        self.config_store.repo_path(project, repo, &self.home).map(|_| ()).map_err(|e| {
            ApiError::with_body(409, json!({ "error": e.message, "code": "repo-not-found", "project": e.project, "repo": e.repo }))
        })
    }

    fn record_session(&self, auth: &AuthContext) {
        self.hub.record_relayed_session(auth);
    }

    fn scope_key_for(&self, feature_id: &str, payload: &Value) -> Option<String> {
        crate::scope_key::scope_key_for(&self.packs, feature_id, payload, git_remote_rules())
    }

    fn history_metrics(&self, days: i64) -> Option<HookResult> {
        let h = self.history.as_ref()?;
        Some(h.metrics(days, None).map(|m| json!({ "days": days, "metrics": m })).map_err(|e| ApiError::new(500, &e)))
    }

    fn targets(&self) -> Value {
        let config = (self.config)();
        let t = crate::targets::analyze_targets_from(&config);
        let mut targets = json!({ "projects": t.projects, "issueTypes": t.issue_types });
        if let (Some(extra), Some(t)) = (self.packs.targets_for(&config).as_object(), targets.as_object_mut()) {
            for (k, v) in extra {
                t.insert(k.clone(), v.clone());
            }
        }
        targets
    }

    fn analysis_cache(&self, issue_key: &str) -> Option<String> {
        crate::features::analyze_issue::plan::read_analysis_cache(issue_key, &self.cache_root)?["completedAt"].as_str().map(String::from)
    }

    fn summary_cache(&self, issue_key: &str) -> Option<(String, Value)> {
        let e = crate::features::summarize_comments::plan::read_summary_cache(issue_key, &self.cache_root).ok().flatten()?;
        Some((e.completed_at, e.comment_count))
    }

    fn heartbeat_hosts(&self) -> Value {
        let (heartbeat, hosts) = auth_context::heartbeat_hosts(&(self.config)());
        json!({ "heartbeat": heartbeat, "hosts": hosts })
    }

    fn configured_origins(&self) -> Vec<String> {
        auth_context::configured_origins(&(self.config)())
    }

    async fn update_info(&self, refresh: bool) -> HookResult {
        let loc = crate::updater::Locations { companion_service_dir: self.service_dir.clone(), home: self.home.clone() };
        let opts = crate::updater::UpdateInfoOptions { refresh, under_launchd: self.under_launchd, running_version: self.version.clone() };
        Ok(crate::updater::get_update_info(&self.checker, &loc, opts).await)
    }

    fn under_launchd(&self) -> bool {
        self.under_launchd
    }

    fn started_at(&self) -> String {
        self.started_at.clone()
    }

    async fn data_folder(&self) -> HookResult {
        self.cfg.data_folder()
    }

    async fn data_folder_open(&self) -> HookResult {
        self.cfg.data_folder_open().await
    }

    async fn data_folder_purge(&self) -> HookResult {
        self.cfg.data_folder_purge()
    }

    async fn open_location(&self, body: &Value) -> HookResult {
        self.cfg.open_location(body).await
    }

    async fn pr_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: Option<i64>) -> HookResult {
        self.cfg.pr_status(auth, project, repo, pr_id).await
    }

    fn update_apply(&self) -> HookResult {
        self.cfg.update_apply()
    }

    async fn settings_get(&self) -> HookResult {
        self.cfg.settings_get().await
    }

    async fn settings_put(&self, body: &Value) -> HookResult {
        self.cfg.settings_put(body).await
    }

    async fn repos_choose(&self, body: &Value) -> HookResult {
        self.cfg.repos_choose(body).await
    }

    fn repos_clone(&self, body: &Value, origin: Option<&str>) -> HookResult {
        self.cfg.repos_clone(body, origin)
    }

    fn manual_start_command(&self) -> String {
        self.cfg.manual_start_command()
    }

    async fn open_claude_code(&self, dir: &str) -> Result<(), String> {
        self.cfg.open_claude_code(dir).await
    }

    async fn resume_session(&self, session: &Value, header: &[Vec<crate::terminal::HeaderSegment>], job_worktree: Option<&str>) -> Result<server::ResumeResult, String> {
        self.cfg.resume_session(session, header, job_worktree).await
    }

    fn restart_process(&self) {
        // SIGTERM ourselves: launchd's KeepAlive starts the new build.
        // SAFETY: signalling our own process.
        unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    }
}

/// Adapts `mcp::handle_mcp` to the HTTP layer.
struct McpAdapter {
    deps: McpDeps,
}

#[async_trait]
impl HttpMcpHandler for McpAdapter {
    async fn handle(&self, method: &str, host: Option<&str>, authorization: Option<&str>, body: &[u8]) -> (u16, Option<Value>) {
        let req = McpRequest { method, host, authorization };
        let r = if method != "POST" {
            mcp::not_allowed()
        } else if let Some(r) = mcp::gate(&req, &self.deps) {
            r
        } else {
            mcp::handle_mcp(&req, body, &self.deps).await
        };
        (r.status, r.body)
    }
}

/// Starts a feature job the way a click would: the server's one start path.
struct Starter {
    state: std::sync::OnceLock<Arc<AppState>>,
}

#[async_trait]
impl StartFeatureJob for Starter {
    async fn start_feature_job(&self, feature: Arc<dyn Feature>, payload: Value, ctx: FeatureContext, via: StartVia, watcher: Option<&str>) -> Result<Value, String> {
        let state = self.state.get().ok_or("The service is still starting.")?;
        server::start_feature_job(state, &feature, payload, ctx, via, watcher).await.map_err(|e| match e {
            server::StartError::Api(a) => a.body["error"].as_str().unwrap_or("The job couldn't be started.").to_string(),
            server::StartError::Feature(f) => f.to_string(),
        })
    }
}

/// The shared history as the recorder's store.
struct SharedHistory(Arc<HistoryDb>);

impl crate::history_record::History for SharedHistory {
    fn upsert_item(&self, item: &crate::history_record::ItemOp) -> Result<i64, String> {
        crate::history_record::History::upsert_item(self.0.as_ref(), item)
    }
    fn add_edge(&self, src: i64, dst: i64, rel: &str, at: i64) -> Result<(), String> {
        crate::history_record::History::add_edge(self.0.as_ref(), src, dst, rel, at)
    }
    fn record_event(&self, event: &Value) -> Result<(), String> {
        crate::history_record::History::record_event(self.0.as_ref(), event)
    }
}

fn open_history(file: &Path) -> Result<Arc<HistoryDb>, String> {
    HistoryDb::open(file, git_remote_rules()).map(Arc::new)
}

/// Builds the service. `Ok(None)` when there is no config yet (the caller exits 0, so launchd
/// doesn't respawn it: setup restarts the service once it has written config.json).
pub fn build(dist: Distribution) -> Result<Option<Running>, String> {
    let Distribution { packs, home, config_path, version, extra_mcp_handlers, update_runner, overrides } = dist;
    let registry = Arc::new(FeatureRegistry::from_packs(&packs));
    let config_store = match ConfigStore::load(&config_path, Some(&registry)) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("[startup] FAIL - {e}");
            return Ok(None);
        }
    };
    let config: ConfigFn = {
        let s = config_store.clone();
        Arc::new(move || s.snapshot())
    };
    let state_dir = crate::paths::state_dir(&home);
    let service_dir = config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| home.clone());
    let stable_install = overrides.stable_install.unwrap_or_else(|| is_installed_copy(&service_dir, &home));

    // One-time move off the old state dir, for the installed copy only.
    if crate::paths::should_migrate(stable_install, std::env::var("AI_DEV_COMPANION_SKIP_MIGRATION").ok().as_deref()) {
        let m = crate::paths::migrate_state_dir(&home, &crate::paths::default_git, &|l| println!("{l}"));
        if m.migrated {
            ok(&format!("moved state from {} to {}", crate::paths::legacy_state_dir(&home).display(), state_dir.display()));
        }
        for e in &m.errors {
            warn(e);
        }
    } else {
        ok("skipped state-dir migration (not the installed copy)");
    }

    // Credentials, and any token still sitting in config.json moved into the store.
    let creds = Arc::new(Credentials::open(service_dir.join("credentials.enc"), state_dir.join("credentials.key")));
    {
        let mut cfg = config_store.snapshot();
        let migrated = crate::settings::migrate_tokens_to_store(
            &mut cfg,
            |name, value| creds.set_token(name, Some(value)).map_err(|e| e.to_string()),
            |c| config_store.replace(c.clone()),
            warn,
        );
        if !migrated.is_empty() {
            ok(&format!("moved {} out of config.json into the encrypted credential store.", migrated.join(", ")));
        }
    }

    let enabled_ids = registry.enabled_feature_ids(&config());
    if !overrides.skip_startup_checks {
        let ids: Vec<&str> = enabled_ids.iter().map(String::as_str).collect();
        for name in registry.required_checks_for(&ids) {
            if let Some(r) = prereqs::run_check_by_name(&name) {
                println!("[startup] {} - {}", if r.ok { "OK  " } else { "WARN" }, r.message);
            }
        }
        for (key, path) in config().get("repos").and_then(Value::as_object).into_iter().flatten() {
            let r = prereqs::check_repo_path(path.as_str().unwrap_or(""));
            println!("[startup] {} - repos[\"{key}\"]: {}", if r.ok { "OK  " } else { "WARN" }, r.message);
        }
    }

    // Local history: installed copy only (a dev checkout shares the state dir).
    let history_env = std::env::var("AI_DEV_COMPANION_HISTORY").ok();
    let history_on = history_env.as_deref() == Some("1") || (history_env.as_deref() != Some("0") && stable_install);
    let store = Arc::new(JobStore::new());
    let history_rules = HistoryRules::from_registry(&packs, git_remote_rules());
    let mut history: Option<Arc<HistoryDb>> = None;
    let mut recorder: Option<Arc<Recorder>> = None;
    if history_on {
        let file = state_dir.join("history.db");
        match open_history(&file) {
            Ok(h) => {
                let rules = HistoryRules::from_registry(&packs, git_remote_rules());
                let r = Arc::new(Recorder::new(Box::new(SharedHistory(h.clone())), rules, Box::new(|m| println!("[history] WARN - {m}")), Box::new(|| crate::job_files::now_ms() as i64)));
                let on = r.clone();
                store.on_transition(move |job, _from| on.on_transition(job));
                recorder = Some(r);
                ok(&format!("local history is on ({})", file.display()));
                history = Some(h);
            }
            Err(e) => warn(&format!("couldn't open the local history: {e}")),
        }
    } else {
        ok("local history is off (not the installed copy)");
    }

    // The sign-in chain and the providers.
    let tokens_lookup: Arc<dyn TokenLookup> = Arc::new(CredentialTokens(creds.clone()));
    let hub = Arc::new(AuthHub::new(tokens_lookup.clone()));
    let shared: SharedConfig = Arc::new(RwLock::new(config()));
    hub.configure_session_vault(shared.clone());
    let mut http_deps: Option<Deps<ReqwestTransport>> = None;
    let providers = match overrides.providers.clone() {
        Some(p) => p,
        None => {
            let sources: Vec<_> = packs.external_token_sources().into_iter().cloned().collect();
            let tokens = Arc::new(SiteTokens::new(tokens_lookup.clone(), ExternalTokens::new(home.clone(), sources)));
            let client = Arc::new(Client::new(ReqwestTransport::new()?, hub.clone()));
            let deps = Deps { client, tokens, config: config.clone() };
            http_deps = Some(deps.clone());
            create_providers(deps)?
        }
    };
    let claude: Arc<dyn ClaudeRunner> = match overrides.claude.clone() {
        Some(c) => c,
        None => Arc::new(SystemClaude::new(ClaudeEnv::for_current_exe(&state_dir.to_string_lossy(), &home.to_string_lossy()).map_err(|e| e.to_string())?)),
    };
    // The LLM the pack provides (none: triage by rules and similar items by words).
    let llm: Arc<dyn LlmBackend> = match (&overrides.llm, packs.llm_factory()) {
        (Some(l), _) => l.clone(),
        (None, Some(build)) => {
            let (c, creds) = (config.clone(), creds.clone());
            build(&LlmContext { config: c, get_token: Arc::new(move |name| creds.get_token(name)), os_user: std::env::var("USER").unwrap_or_default() })?
        }
        (None, None) => Arc::new(NoLlm),
    };
    let _ = &shared;
    // The shared risk-facts client (diagnose-build's flaky lines and `get_change_risk` share its
    // cache) and the similar-item search over the history and the LLM.
    let risk_facts = crate::risk_facts::RiskFactsClient::new(ReqwestTransport::new()?);
    let find_similar: Option<Arc<dyn crate::jobs::FindSimilar>> = history.clone().map(|h| {
        Arc::new(crate::mcp_feature_tools::OwnedSimilar { history: h, llm: LlmHandle(llm.clone()), config: config.clone(), rules: git_remote_rules() }) as Arc<dyn crate::jobs::FindSimilar>
    });

    // The shared services every feature is built from.
    let cache_root = service_dir.clone();
    let repo_path: crate::features::review_in_editor::index::RepoResolver = {
        let (s, h) = (config_store.clone(), home.clone());
        Arc::new(move |p, r| s.repo_path(p, r, &h).map_err(|e| e.message))
    };
    let services = FeatureServices {
        http_deps: http_deps.clone(),
        store: store.clone(),
        providers: providers.clone(),
        claude,
        config: config.clone(),
        home: home.clone(),
        cache_root: cache_root.clone(),
        rules: git_remote_rules(),
        work_units: Arc::new(WorkUnits::open(state_dir.join("work-units.json"))),
        choices: Arc::new(RepoChoices::new(store.clone())),
        baselines: Arc::new(BaselineStore::new(None)),
        repo_path,
        history: history.clone(),
        find_similar: find_similar.clone(),
        save_component: {
            let s = config_store.clone();
            Arc::new(move |component: &str, repo_key: &str| s.save_component_repo(component, repo_key))
        },
        facts: Arc::new(risk_facts.clone()),
        richer_diagnose: {
            let p = packs.clone();
            Arc::new(move || p.diagnose_command().cloned())
        },
        deny_list: packs.disallowed_mcp_tools().into_iter().map(String::from).collect(),
        server_hints: packs.analysis_server_hints().into_iter().cloned().collect(),
        mcp_servers: {
            let path = home.join(".claude.json");
            Arc::new(move || crate::features::analyze_issue::plan::read_mcp_servers(&path))
        },
        terminal: Terminal::default(),
        editors: crate::editor::Editors::default(),
        sessions_dir: crate::review_session::state_dir(&state_dir).map_err(|e| e.to_string())?,
        memory_dir: state_dir.clone(),
        permission_mode: Arc::new(crate::permission_mode::permission_mode_for_current),
        osascript: "osascript".into(),
        cursor_command: "cursor".into(),
    };
    let features: Vec<Arc<dyn Feature>> = enabled_ids.iter().filter_map(|id| packs.feature_spec(id)).map(|spec| (spec.factory)(&services)).collect();

    // Jobs persisted across restarts (installed copy only).
    let persist_env = std::env::var("AI_DEV_COMPANION_PERSIST_JOBS").ok();
    let persist_jobs = persist_env.as_deref() == Some("1") || (persist_env.as_deref() != Some("0") && stable_install);
    if persist_jobs {
        let dir = crate::job_files::jobs_dir(&state_dir.to_string_lossy());
        let repo_paths = |c: &Value| -> Vec<String> { c["repos"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default() };
        let (cfg_a, cfg_b, home2) = (config.clone(), config.clone(), home.clone());
        let sessions_root = state_dir.join("sessions").to_string_lossy().into_owned();
        let checkouts_root = state_dir.join("repos").to_string_lossy().into_owned();
        let not_persisted: Vec<&str> = packs.not_persisted_ids();
        let ids: Vec<&str> = features.iter().map(|f| f.id()).collect();
        let feature_ids = crate::job_files::feature_ids_to_persist(&ids, &not_persisted).into_iter().map(String::from).collect();
        let worktree_root = crate::worktree::worktree_root(&home).to_string_lossy().into_owned();
        let load = store.enable_persistence(PersistenceOptions {
            dir: &dir,
            worktree_root: &worktree_root,
            feature_ids,
            adopted_worktree_ok: &move |repo, dir| repo_paths(&cfg_a()).iter().any(|p| crate::node_path::resolve(p) == crate::node_path::resolve(repo)) && crate::worktree_registry::is_linked_worktree_of(repo, dir),
            session_cwd_ok: &move |cwd| {
                let paths = repo_paths(&cfg_b());
                crate::session_cwd::session_cwd_allowed(Some(cwd), &crate::session_cwd::SessionCwdRules { repo_paths: &paths, sessions_root: Some(&sessions_root), checkouts_root: Some(&checkouts_root), worktrees_root: None }, &home2)
            },
        });
        ok(&format!("restored {} job(s) ({} interrupted, {} pruned, {} unreadable)", load.loaded, load.reconciled.len(), load.pruned.len(), load.dropped.len()));
        // Worktree-integrity baselines live beside the job files.
        services.baselines.set_dir(Some(PathBuf::from(&dir)));
        // The jobs that aged out while the service was down: remove their worktrees and note it.
        if let (false, Ok(runtime)) = (load.pruned_jobs.is_empty(), tokio::runtime::Handle::try_current()) {
            let (jobs, home, history, rules) = (load.pruned_jobs.clone(), home.clone(), history.clone(), history_rules.clone());
            runtime.spawn(async move {
                for job in jobs {
                    crate::maintenance::clean_up_expired_job(&job, &home, history.as_deref(), &rules, &|m| println!("{m}")).await;
                }
            });
        }
    } else {
        ok("jobs are kept in memory only (not the installed copy)");
    }

    // The inbox, the scheduler, the watchers and the digest.
    let inbox = Arc::new(Mutex::new(NotificationStore::new(persist_jobs.then(|| state_dir.join("notifications.json")))));
    let starter = Arc::new(Starter { state: std::sync::OnceLock::new() });
    let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let maintenance: TaskFn = {
        let (store, history, config, home, rules) = (store.clone(), history.clone(), config.clone(), home.clone(), history_rules.clone());
        crate::scheduler::task_fn(move |_| {
            let (store, history, config, home, rules) = (store.clone(), history.clone(), config.clone(), home.clone(), rules.clone());
            async move {
                let days = config()["history"]["retentionDays"].as_i64().unwrap_or(180);
                let out = crate::maintenance::daily_maintenance(&store, &home, history.as_deref(), &rules, days, &|m| println!("{m}")).await;
                Ok(crate::background_wiring::task_result_from_value(&out))
            }
        })
    };
    let wiring = create_background_wiring(BackgroundWiringDeps {
        config: config.clone(),
        features: features.clone(),
        job_store: store.clone(),
        start_feature_job: starter.clone(),
        inbox: inbox.clone(),
        history: history.clone(),
        llm: Arc::new(LlmHandle(llm.clone())),
        io: Arc::new(crate::watcher_runner::SystemWatcherIo {
            providers: providers.clone(),
            config: config.clone(),
            analysis_cached: {
                let root = cache_root.clone();
                Arc::new(move |k| crate::features::analyze_issue::plan::read_analysis_cache(k, &root).is_some())
            },
            git: Default::default(),
        }),
        digest_io: Arc::new(ProviderDigestIo { providers: providers.clone(), jobs: store.clone() }),
        clock: None,
        state_path: persist_jobs.then(|| state_dir.join("scheduler.json")),
        maintenance,
        is_shutting_down: Some({
            let s = shutting_down.clone();
            Arc::new(move || s.load(std::sync::atomic::Ordering::SeqCst))
        }),
        git: git_remote_rules(),
        base_urls: {
            let p = providers.clone();
            Arc::new(move || p.git.base_url().into_iter().chain(Some(p.issues.base_url())).collect())
        },
        log: None,
    });

    // The HTTP hooks and /mcp.
    let port = overrides.port.unwrap_or_else(|| config()["port"].as_u64().unwrap_or(7777) as u16);
    let open_location = Arc::new(crate::open_location::OpenLocation::new(
        Arc::new(crate::open_location::SystemOpener::default()),
        prereqs::detect_installed_editors().iter().map(|e| e.id.to_string()).collect(),
        git_remote_rules(),
        true,
    ));
    let checker = crate::updater::UpdateChecker::new(home.clone());
    let under_launchd = std::env::var("AI_DEV_COMPANION_UNDER_LAUNCHD").as_deref() == Ok("1");
    let started_at = crate::js_date::to_iso_string(crate::job_files::now_ms() as i64).unwrap_or_default();
    let cfg = Arc::new(ConfigHooks {
        config_store: config_store.clone(),
        config: config.clone(),
        home: home.clone(),
        packs: packs.clone(),
        registry: registry.clone(),
        creds: creds.clone(),
        store: store.clone(),
        providers: providers.clone(),
        llm,
        scheduler: wiring.scheduler.clone(),
        enabled_ids: enabled_ids.clone(),
        version: version.clone(),
        started_at: started_at.clone(),
        under_launchd,
        service_dir: service_dir.clone(),
        checker: checker.clone(),
        installer: update_runner,
        terminal: Terminal::default(),
        open_location: open_location.clone(),
        repo_setup: Arc::new(crate::repo_setup::RepoSetup::new(config_store.clone(), store.clone(), git_remote_rules(), &home)),
        external_tokens: ExternalTokens::new(home.clone(), packs.external_token_sources().into_iter().cloned().collect()),
        pr_status: crate::hooks_config::pr_status_for(&providers),
        folder_opener: ConfigHooks::default_folder_opener(),
    });
    let hooks = Arc::new(ServiceHooks {
        cfg: cfg.clone(),
        config_store: config_store.clone(),
        config: config.clone(),
        home: home.clone(),
        packs: packs.clone(),
        registry: registry.clone(),
        readiness: Readiness { registry: registry.clone(), passed: Mutex::new(HashMap::new()) },
        hub,
        cache_root,
        history: history.clone(),
        version: version.clone(),
        started_at,
        under_launchd,
        service_dir: service_dir.clone(),
        checker,
    });
    let catalog = crate::mcp_tools::tool_catalog(&packs)?;
    let mcp_deps = McpDeps {
        config: config.clone(),
        home: home.clone(),
        version: version.clone().unwrap_or_default(),
        port,
        git: git_remote_rules(),
        get_token: {
            let c = creds.clone();
            Arc::new(move || Ok(c.get_token("mcp.token")))
        },
        store: store.clone(),
        providers: providers.clone(),
        catalog,
        features: registry.clone(),
        enabled_feature_ids: enabled_ids.clone(),
        history: history.clone(),
        start_feature_job: {
            let (st, feats) = (starter.clone(), features.clone());
            Arc::new(move |id: String, payload: Value| {
                let (st, feature) = (st.clone(), feats.iter().find(|f| f.id() == id).cloned());
                Box::pin(async move {
                    let feature = feature.ok_or_else(|| format!("{id} isn't enabled."))?;
                    st.start_feature_job(feature, payload, FeatureContext::default(), StartVia::Mcp, None).await
                })
            })
        },
        read_mcp_servers: services.mcp_servers.clone(),
        notifications: Some(inbox.clone()),
        open_location: Some(open_location),
        scope_key_for: {
            let p = packs.clone();
            Arc::new(move |id, payload| crate::scope_key::scope_key_for(&p, id, payload, git_remote_rules()))
        },
        extra_handlers: {
            // The feature-owned tools, then the distribution's own (which may replace one).
            let mut handlers = crate::mcp_feature_tools::feature_tool_handlers(crate::mcp_feature_tools::FeatureToolDeps {
                providers: providers.clone(),
                store: store.clone(),
                config: config.clone(),
                home: home.clone(),
                rules: git_remote_rules(),
                cache_root: service_dir.clone(),
                history: history.clone(),
                digest_io: Arc::new(ProviderDigestIo { providers: providers.clone(), jobs: store.clone() }),
                find_similar: find_similar.clone(),
                risk_facts: Some(risk_facts.clone()),
            });
            handlers.extend(extra_mcp_handlers);
            handlers
        },
    };
    let state = Arc::new(AppState::new(port, store.clone(), features.clone(), hooks).with_background(wiring.routes.clone()).with_mcp(Arc::new(McpAdapter { deps: mcp_deps })).with_addons(packs.job_addons(), config.clone()));
    let _ = starter.state.set(state.clone());
    let running = Running {
        router: server::app(state.clone()),
        state,
        port,
        shutdown: Arc::new(Shutdown { store, recorder, scheduler: wiring.scheduler.clone(), stop_background: Mutex::new(vec![]), shutting_down }),
    };
    running.start_background(persist_jobs, &features);
    if let Some(stop) = cfg.start_update_checks() {
        running.shutdown.stop_background.lock().unwrap().push(stop);
    }
    let _ = version;
    Ok(Some(running))
}

/// Runs the service until SIGINT/SIGTERM. A missing config exits cleanly (see [`build`]).
pub async fn run_service(dist: Distribution) -> Result<(), String> {
    let Some(running) = build(dist)? else { return Ok(()) };
    let port = running.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("port {port} is already in use by another process. Either stop that process (`lsof -nP -iTCP:{port} -sTCP:LISTEN` shows which), or change \"port\" in config.json and re-run setup so the extension picks up the new port.")
        } else {
            format!("could not start the HTTP server: {e}")
        }
    })?;
    println!("listening on http://127.0.0.1:{port}");
    let signal = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("a SIGTERM handler installs");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    };
    axum::serve(listener, running.router.clone()).with_graceful_shutdown(signal).await.map_err(|e| e.to_string())?;
    running.shutdown().await;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::features::declare::SetupContext;
    use axum::body::Body;
    use tower::ServiceExt;

    const SECRET: &str = "s3cret-for-tests";

    struct Fx {
        _d: tempfile::TempDir,
        dist: Distribution,
        home: PathBuf,
    }

    fn fx(config: Value) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(d.path()).unwrap();
        let service = home.join("service");
        std::fs::create_dir_all(&service).unwrap();
        let config_path = service.join("config.json");
        std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();
        let pack = crate::packs::builtin::pack(&SetupContext { token_detector: None, token_tool: String::new() }).unwrap();
        let dist = Distribution {
            packs: Arc::new(PackRegistry::new(vec![pack]).unwrap()),
            home: home.clone(),
            config_path,
            version: Some("0.0.0-test".into()),
            extra_mcp_handlers: HashMap::new(),
            update_runner: None,
            overrides: Overrides { stable_install: Some(false), port: Some(8787), skip_startup_checks: true, ..Default::default() },
        };
        Fx { _d: d, dist, home }
    }

    fn base_config() -> Value {
        json!({ "port": 8787, "sharedSecret": SECRET, "repos": {} })
    }

    async fn get(router: &Router, uri: &str, secret: Option<&str>) -> (u16, Value) {
        let mut b = axum::http::Request::builder().uri(uri).header("host", "127.0.0.1:8787");
        if let Some(s) = secret {
            b = b.header("x-companion-secret", s);
        }
        let res = router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_missing_config_is_not_an_error_so_launchd_does_not_respawn_the_service() {
        let f = fx(base_config());
        std::fs::remove_file(&f.dist.config_path).unwrap();
        assert!(build(f.dist).unwrap().is_none());
    }

    #[tokio::test]
    async fn the_built_service_answers_health_and_an_authed_features_list_then_shuts_down() {
        let f = fx(base_config());
        let running = build(f.dist).unwrap().expect("a service");
        assert_eq!(running.port, 8787);
        let (s, _) = get(&running.router, "/health", None).await;
        assert_eq!(s, 200);
        assert_eq!(get(&running.router, "/features", None).await.0, 403);
        assert_eq!(get(&running.router, "/features", Some("wrong")).await.0, 403);
        let (s, features) = get(&running.router, "/features", Some(SECRET)).await;
        assert_eq!(s, 200, "{features}");
        assert!(features.to_string().contains("diagnose-build"), "{features}");
        // A job that doesn't exist.
        assert_eq!(get(&running.router, "/status/nope", Some(SECRET)).await.0, 404);
        running.shutdown().await;
    }

    #[test]
    fn the_installed_copy_is_the_companion_service_folder_inside_the_install_dir() {
        let d = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(d.path()).unwrap();
        let installed = crate::updater::stable_install_dir(&home).join("companion-service");
        std::fs::create_dir_all(&installed).unwrap();
        assert!(is_installed_copy(&installed, &home), "the service's own folder under ~/<slug> is the installed copy");
        let checkout = home.join("checkout").join("companion-service");
        std::fs::create_dir_all(&checkout).unwrap();
        assert!(!is_installed_copy(&checkout, &home));
        assert!(!is_installed_copy(&home.join("service"), &home));
    }

    #[tokio::test]
    async fn the_installed_copy_keeps_history_and_jobs_on_disk() {
        let mut f = fx(base_config());
        let service = crate::updater::stable_install_dir(&f.home).join("companion-service");
        std::fs::create_dir_all(&service).unwrap();
        std::fs::write(service.join("config.json"), serde_json::to_string(&base_config()).unwrap()).unwrap();
        f.dist.config_path = service.join("config.json");
        f.dist.overrides.stable_install = None;
        let home = f.home.clone();
        let running = build(f.dist).unwrap().unwrap();
        running.shutdown().await;
        assert!(crate::paths::state_dir(&home).join("history.db").exists(), "history is on for the installed copy");
    }

    #[tokio::test]
    async fn a_dev_checkout_keeps_jobs_and_history_off_the_disk() {
        let f = fx(base_config());
        let home = f.home.clone();
        let running = build(f.dist).unwrap().unwrap();
        running.shutdown().await;
        assert!(!crate::paths::state_dir(&home).join("history.db").exists());
        assert!(!crate::paths::state_dir(&home).join("notifications.json").exists());
    }

    #[tokio::test]
    async fn the_mcp_route_is_gated_by_its_own_token_not_the_shared_secret() {
        let f = fx(base_config());
        let running = build(f.dist).unwrap().unwrap();
        let req = axum::http::Request::builder().method("POST").uri("/mcp").header("host", "127.0.0.1:8787").header("x-companion-secret", SECRET).header("content-type", "application/json").body(Body::from("{}")).unwrap();
        let res = running.router.clone().oneshot(req).await.unwrap();
        // No MCP token is saved in this temp home, so the endpoint is not available.
        assert!(matches!(res.status().as_u16(), 401 | 403 | 503), "{}", res.status());
        running.shutdown().await;
    }

    #[tokio::test]
    async fn run_service_refuses_a_port_that_is_already_taken() {
        let taken = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = taken.local_addr().unwrap().port();
        let mut f = fx(base_config());
        f.dist.overrides.port = Some(port);
        let e = run_service(f.dist).await.unwrap_err();
        assert!(e.contains(&format!("port {port} is already in use")), "{e}");
    }
}
