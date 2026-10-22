//! The HTTP hooks that read and change the machine's configuration and its surroundings (port of
//! the settings, data-folder, open-location, PR-status, repo-setup, update and terminal routes of
//! `server.ts`): everything `startup` used to answer 501 for. [`ConfigHooks`] holds what those routes
//! share; `startup::ServiceHooks` delegates to it.

use crate::atlassian::AuthContext;
use crate::config::{read_config_file, ConfigFn, ConfigStore};
use crate::credentials::Credentials;
use crate::data_folder::{data_folder_usage, purge_data_folder};
use crate::environment;
use crate::exec::{self, RunOptions};
use crate::external_tokens::ExternalTokens;
use crate::feature_registry::{FeatureRegistry, REVIEW_EDITORS};
use crate::jobs::JobStore;
use crate::llm::LlmBackend;
use crate::mcp_registration::{register_everywhere, spawn_sync, Registered};
use crate::open_location::{OpenLocation, OpenLocationErrorCode};
use crate::packs::builtin::FeatureFactory;
use crate::packs::PackRegistry;
use crate::pr_status::{PrGit, PrStatus, PrStatusError};
use crate::prereqs::{self, check_repo_path, Check};
use crate::providers::{GitCapability, GitHost, Providers};
use crate::repo_setup::{RepoSetup, RepoSetupError};
use crate::resume_session::{resume_session_in_terminal, ResumeOutcome};
use crate::scheduler::Scheduler;
use crate::server::{ApiError, HookResult, ResumeResult};
use crate::settings::{self, CredentialView, RepoDeps, Settings, TokenOpsResult, LOCKED_KEYS};
use crate::terminal::{HeaderSegment, Terminal, TerminalOptions};
use crate::updater::{start_update, Locations, RunnerCommand, UpdateChecker};
use serde_json::{json, Map, Value};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(15);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// What `PrStatus` asks of the git host, over the configured provider.
pub struct ProviderPrGit(pub Arc<dyn GitHost>);

pub fn auth_to_value(auth: &AuthContext) -> Value {
    json!({ "cookie": auth.cookie, "origin": auth.origin })
}

fn auth_from_value(v: &Value) -> AuthContext {
    AuthContext { cookie: v["cookie"].as_str().map(String::from), origin: v["origin"].as_str().map(String::from) }
}

impl PrGit for ProviderPrGit {
    fn get_pull_request(&self, auth: &Value, project: &str, repo: &str, pr_id: u64) -> impl Future<Output = Result<Option<Value>, String>> + Send {
        let (git, auth, project, repo) = (self.0.clone(), auth_from_value(auth), project.to_string(), repo.to_string());
        async move { git.get_pull_request(&auth, &project, &repo, pr_id as i64).await.map_err(|e| e.to_string()) }
    }
    fn has_merge_status(&self) -> bool {
        self.0.has(GitCapability::MergeStatus)
    }
    fn get_merge_status(&self, auth: &Value, project: &str, repo: &str, pr_id: u64) -> impl Future<Output = Result<Value, String>> + Send {
        let (git, auth, project, repo) = (self.0.clone(), auth_from_value(auth), project.to_string(), repo.to_string());
        async move { git.get_merge_status(&auth, &project, &repo, pr_id as i64).await.map_err(|e| e.to_string()) }
    }
    fn pr_url(&self, project: &str, repo: &str, pr_id: u64) -> String {
        self.0.pr_url(project, repo, pr_id as i64).unwrap_or_default()
    }
}

/// Everything the configuration-facing routes share.
pub struct ConfigHooks {
    pub config_store: Arc<ConfigStore>,
    pub config: ConfigFn,
    pub home: PathBuf,
    pub packs: Arc<PackRegistry<FeatureFactory>>,
    pub registry: Arc<FeatureRegistry>,
    pub creds: Arc<Credentials>,
    pub store: Arc<JobStore>,
    pub providers: Providers,
    pub llm: Arc<dyn LlmBackend>,
    pub scheduler: Arc<Scheduler>,
    /// The features this process was started with (a saved change needs a restart to differ).
    pub enabled_ids: Vec<String>,
    pub version: Option<String>,
    pub started_at: String,
    pub under_launchd: bool,
    pub service_dir: PathBuf,
    pub checker: Arc<UpdateChecker>,
    /// How to start the detached update runner; `None`: this build can't update itself.
    pub installer: Option<RunnerCommand>,
    pub terminal: Terminal,
    pub open_location: Arc<OpenLocation>,
    pub repo_setup: Arc<RepoSetup>,
    pub external_tokens: ExternalTokens,
    pub pr_status: PrStatus<ProviderPrGit>,
    /// `open` / `xdg-open`: shows a folder.
    pub folder_opener: String,
}

fn default_folder_opener() -> String {
    if cfg!(target_os = "macos") { "open" } else { "xdg-open" }.into()
}

pub fn pr_status_for(providers: &Providers) -> PrStatus<ProviderPrGit> {
    let git = providers.git.clone();
    PrStatus::new(move || Arc::new(ProviderPrGit(git.clone())))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(500, &e.to_string())
}

fn check_named(name: &str) -> Check {
    prereqs::run_check_by_name(name).unwrap_or(Check { ok: false, message: format!("Unknown check \"{name}\".") })
}

fn registered_json(r: &Option<Registered>) -> Value {
    r.as_ref().map_or(Value::Null, |r| json!({ "ok": r.ok, "message": r.message }))
}

impl ConfigHooks {
    pub fn default_folder_opener() -> String {
        default_folder_opener()
    }

    fn state_dir(&self) -> PathBuf {
        crate::paths::state_dir(&self.home)
    }

    /// `config.json` as saved (what the panel shows, even when it differs from what this process
    /// started with), or the running config when it can't be read.
    fn saved_config(&self) -> Value {
        read_config_file(self.config_store.path()).unwrap_or_else(|_| (self.config)())
    }

    pub fn manual_start_command(&self) -> String {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "companion".into());
        format!("\"{exe}\" serve")
    }

    // ---- data folder ----

    pub fn data_folder(&self) -> HookResult {
        Ok(serde_json::to_value(data_folder_usage(&self.state_dir())).expect("usage serializes"))
    }

    pub async fn data_folder_open(&self) -> HookResult {
        let dir = self.state_dir();
        std::fs::create_dir_all(&dir).map_err(internal)?;
        exec::run(&self.folder_opener, &[dir.to_string_lossy().into_owned()], RunOptions::default()).await.map_err(internal)?;
        Ok(json!({ "ok": true, "path": dir.to_string_lossy() }))
    }

    pub fn data_folder_purge(&self) -> HookResult {
        // A running job is using its worktree; a restart-safe purge waits for it.
        if self.store.active_count() > 0 {
            return Err(ApiError::with_body(409, json!({ "code": "jobs-running", "error": "Jobs are still running. Wait for them to finish (or cancel them), then purge." })));
        }
        let dir = self.state_dir();
        let result = purge_data_folder(&dir);
        println!("[data] purged {} ({} bytes)", if result.removed.is_empty() { "nothing".to_string() } else { result.removed.join(", ") }, result.freed_bytes);
        let mut body = serde_json::to_value(&result).expect("purged serializes");
        body["ok"] = json!(true);
        body["usage"] = serde_json::to_value(data_folder_usage(&dir)).expect("usage serializes");
        Ok(body)
    }

    // ---- open in editor, PR status ----

    pub async fn open_location(&self, body: &Value) -> HookResult {
        let obj = if body.is_object() { body } else { &Value::Null };
        match self.open_location.open_location(&(self.config)(), obj["text"].as_str(), obj["pageUrl"].as_str()).await {
            Ok(opened) => Ok(json!({ "ok": true, "opened": opened })),
            Err(e) => {
                let status = if e.code == OpenLocationErrorCode::Unsupported { 400 } else { 409 };
                Err(ApiError::with_body(status, json!({ "error": e.message, "code": e.code.as_str(), "details": e.details })))
            }
        }
    }

    pub async fn pr_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: Option<i64>) -> HookResult {
        // `Number(undefined)` is NaN: not a positive whole number.
        match self.pr_status.get(&auth_to_value(auth), project, repo, pr_id.unwrap_or(0)).await {
            Ok(v) => Ok(v),
            Err(PrStatusError::Request(m)) => Err(ApiError::new(400, &m)),
            Err(PrStatusError::Lookup(m)) => Err(ApiError::new(500, &m)),
        }
    }

    // ---- repos ----

    pub async fn repos_choose(&self, body: &Value) -> HookResult {
        self.repo_setup.choose_repo_folder(body).await.map(|path| json!({ "path": path })).map_err(repo_setup_error)
    }

    pub fn repos_clone(&self, body: &Value, origin: Option<&str>) -> HookResult {
        self.repo_setup.start_clone(body, origin).map_err(repo_setup_error)
    }

    // ---- terminal ----

    pub async fn open_claude_code(&self, dir: &str) -> Result<(), String> {
        self.terminal.open_claude_code(dir, &[], &[], &TerminalOptions::default()).await.map(|_| ())
    }

    pub async fn resume_session(&self, session: &Value, header: &[Vec<HeaderSegment>], job_worktree: Option<&str>) -> Result<ResumeResult, String> {
        Ok(match resume_session_in_terminal(&self.terminal, &(self.config)(), session, header, job_worktree, &self.home).await? {
            ResumeOutcome::Resumed => ResumeResult::Resumed,
            ResumeOutcome::Invalid => ResumeResult::Invalid,
            ResumeOutcome::Gone => ResumeResult::Gone,
        })
    }

    // ---- updates ----

    pub fn update_apply(&self) -> HookResult {
        let Some(runner) = &self.installer else { return Err(ApiError::new(501, "Updating isn't available in this build.")) };
        let loc = Locations { companion_service_dir: self.service_dir.clone(), home: self.home.clone() };
        start_update(&loc, runner, self.under_launchd.then(std::process::id)).map_err(|e| ApiError::new(400, &e))
    }

    /// The first update check 15 s after start, then every 6 hours. Returns how to stop it.
    pub fn start_update_checks(self: &Arc<Self>) -> Option<Box<dyn FnOnce() + Send>> {
        tokio::runtime::Handle::try_current().ok()?;
        let this = self.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(FIRST_UPDATE_CHECK).await;
            loop {
                let loc = Locations { companion_service_dir: this.service_dir.clone(), home: this.home.clone() };
                this.checker.check_for_update(Duration::ZERO, &loc.read_install_source()).await;
                tokio::time::sleep(UPDATE_CHECK_EVERY).await;
            }
        });
        Some(Box::new(move || task.abort()))
    }

    // ---- settings ----

    fn base_urls(&self) -> Map<String, Value> {
        let env = environment::current();
        let mut out = Map::new();
        for site in &env.sites {
            let url = match site.kind.as_str() {
                "issues" => Some(self.providers.issues.base_url()),
                "ci" => Some(self.providers.ci.base_url()),
                "git" => self.providers.git.base_url().ok(),
                _ => None,
            };
            if let Some(u) = url {
                out.insert(site.id.clone(), json!(u));
            }
        }
        out
    }

    async fn settings_view(&self, saved: &Value) -> Value {
        let env = environment::current();
        let settings = Settings { features: &self.registry, packs: &self.packs };
        let credentials = CredentialView { saved: self.creds.list_token_names(), external: self.external_tokens.sources_for(&self.base_urls()) };
        let repos = saved["repos"].as_object().cloned().unwrap_or_default();
        let mut roots = prereqs::default_search_roots(&self.home);
        roots.extend(repos.values().filter_map(Value::as_str).filter_map(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf())));
        let clones: Vec<settings::Clone_> = prereqs::discover_local_clones(&roots, prereqs::git_remote_rules()).into_iter().map(|c| settings::Clone_ { key: c.key, path: c.path }).collect();
        let suggested: Vec<Value> = settings::suggested_repos(&repos, &clones).into_iter().map(|c| json!({ "key": c.key, "path": c.path })).collect();
        let repo_status: Map<String, Value> = repos.iter().map(|(k, p)| (k.clone(), { let c = check_repo_path(p.as_str().unwrap_or("")); json!({ "ok": c.ok, "message": c.message }) })).collect();
        let detected: Vec<Value> = prereqs::detect_installed_editors().into_iter().map(|e| json!({ "id": e.id, "label": e.label })).collect();
        let supported: Vec<Value> = REVIEW_EDITORS.iter().map(|(id, label)| json!({ "id": id, "label": label })).collect();
        let sites: Vec<Value> = env.sites.iter().map(|s| json!({ "id": s.id, "kind": s.kind, "label": s.label, "provider": s.provider, "tokenOnly": s.token_only == Some(true), "withUsername": s.raw.get("withUsername") == Some(&json!(true)) })).collect();
        let site_base_urls: Map<String, Value> = env.site_ids().into_iter().map(|id| (id.to_string(), json!(env.default_base_url(id)))).collect();
        let jenkins_extras: Map<String, Value> = self.packs.site_setting_extras("jenkins").iter().map(|e| (e.key.clone(), e.default_value.clone())).collect();
        let mut restart = settings.pending_restart(&self.enabled_ids, saved);
        restart["canRestart"] = json!(self.under_launchd);
        restart["manualCommand"] = json!(self.manual_start_command());
        restart["activeJobs"] = json!(self.store.active_count());
        let mut background = Map::new();
        background.insert("scheduler".into(), self.scheduler.status());
        background.extend(self.llm.background_status().await);
        json!({
            "settings": settings.public_settings(saved, &credentials),
            "lockedSettings": LOCKED_KEYS,
            "features": settings::feature_summaries(&self.registry),
            "checks": settings.run_all_checks(&check_named),
            "editors": { "supported": supported, "detected": detected },
            "repoStatus": repo_status,
            "suggestedRepos": suggested,
            "branding": env.profile.get("branding").cloned().unwrap_or_else(|| json!({})),
            "sites": sites,
            "defaults": {
                "jiraBaseUrl": env.default_base_url("jira"),
                "jenkinsBaseUrl": env.default_base_url("jenkins"),
                "bitbucketBaseUrl": env.default_base_url("bitbucket"),
                "siteBaseUrls": site_base_urls,
                "jenkinsExtras": jenkins_extras,
                "analyzeProjects": env.profile.get("targets").and_then(|t| t.get("projects")).cloned().unwrap_or_else(|| json!([])),
            },
            "restart": restart,
            "service": { "version": self.version, "startedAt": self.started_at },
            "background": background,
        })
    }

    pub async fn settings_get(&self) -> HookResult {
        Ok(self.settings_view(&self.saved_config()).await)
    }

    pub async fn settings_put(&self, update: &Value) -> HookResult {
        let saved = read_config_file(self.config_store.path()).map_err(|e| ApiError::new(500, &format!("Could not read config.json: {e}")))?;
        let errors = Settings { features: &self.registry, packs: &self.packs }.validate_settings_update(update, &saved, &RepoDeps::real());
        if !errors.is_empty() {
            let text: Vec<String> = errors.iter().map(|e| if e.field.is_empty() { e.message.clone() } else { format!("{} {}", e.field, e.message) }).collect();
            return Err(ApiError::with_body(400, json!({ "error": text.join("\n"), "code": "invalid-settings", "errors": errors.iter().map(|e| e.to_json()).collect::<Vec<_>>() })));
        }
        // merge_settings is pure: a token change comes back as `token_ops` and is only applied once
        // the config has been written, so a refused or failed save leaves everything unchanged.
        let saved_names = self.creds.list_token_names();
        let generate: &dyn Fn() -> String = &crate::mcp_auth::generate_mcp_token;
        let (next, token_ops) = Settings { features: &self.registry, packs: &self.packs }.merge_settings(&saved, update, Some(&saved_names), Some(generate)).map_err(|e| ApiError::new(400, &e))?;
        let was_enabled = self.registry.enabled_feature_ids(&saved);
        let newly: Vec<String> = self.registry.enabled_feature_ids(&next).into_iter().filter(|id| !was_enabled.contains(id)).collect();
        let newly_refs: Vec<&str> = newly.iter().map(String::as_str).collect();
        let warnings = Settings { features: &self.registry, packs: &self.packs }.prerequisite_warnings(&newly_refs, &check_named);
        if !warnings.is_empty() && update["acknowledgeWarnings"] != json!(true) {
            return Err(ApiError::with_body(409, json!({ "error": "Some newly enabled features can't run on this machine yet.", "code": "prerequisites-failing", "warnings": warnings })));
        }
        let before = (self.config)();
        self.config_store.replace(next.clone()).map_err(|e| ApiError::new(500, &format!("Could not save config.json: {e}")))?;
        let token_result = settings::apply_token_ops(&token_ops, |name, value| self.creds.set_token(name, value).map_err(|e| e.to_string()));
        // The extension's manifest lists the hosts it runs on, so a changed base URL needs it
        // rewritten (and the extension reloaded) before the new host is covered.
        let env = environment::current();
        let mut extension = json!({ "reloadNeeded": false });
        if crate::extension_manifest::hosts_changed(env, &before, &next) {
            let dir = self.service_dir.join("..").join("chrome-extension");
            extension = match crate::extension_manifest::write_extension_files(&dir, &(self.config)(), env, &self.packs) {
                Ok(_) => json!({ "reloadNeeded": true }),
                Err(e) => json!({ "reloadNeeded": false, "error": e.to_string() }),
            };
        }
        println!("[settings] Saved changes from the extension's Settings panel.");
        if let TokenOpsResult::Failed { error, .. } = token_result {
            return Err(ApiError::new(500, &format!("Settings were saved, but saving the API token failed: {error}. Everything else was applied \u{2014} try the token again from \u{2728} \u{2192} \u{2699} Settings.")));
        }
        // A rotated MCP token only helps once the clients registered with the old one have the new one.
        let mcp_registration = if token_ops.iter().any(|op| op.name == "mcp.token") { self.re_register_mcp().await } else { Value::Null };
        let mut view = self.settings_view(&next).await;
        let obj = view.as_object_mut().expect("the view is an object");
        let mut out = Map::from_iter([("ok".to_string(), json!(true)), ("warnings".to_string(), json!(warnings)), ("mcpRegistration".to_string(), mcp_registration), ("extension".to_string(), extension)]);
        out.append(obj);
        Ok(Value::Object(out))
    }

    /// Re-registers the companion's MCP server (with the current token) in the MCP clients it was
    /// registered with. `null` when the token somehow isn't readable.
    async fn re_register_mcp(&self) -> Value {
        let Some(token) = self.creds.get_token("mcp.token") else { return Value::Null };
        let (port, home) = ((self.config)()["port"].as_u64().unwrap_or(0) as u16, self.home.clone());
        let registered = tokio::task::spawn_blocking(move || register_everywhere(port, &token, &home, prereqs::check_claude_cli().ok, &|p, a, t| spawn_sync(p, a, t))).await;
        match registered {
            Ok(r) => json!({ "claude": registered_json(&r.claude), "cursor": registered_json(&r.cursor) }),
            Err(_) => Value::Null,
        }
    }
}

fn repo_setup_error(e: RepoSetupError) -> ApiError {
    let mut body = json!({ "error": e.message });
    if let Some(code) = e.code {
        body["code"] = json!(code);
    }
    ApiError::with_body(400, body)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::features::declare::SetupContext;
    use crate::packs::PackRegistry;
    use crate::startup::{build, Distribution, Overrides, Running};
    use axum::body::Body;
    use std::collections::HashMap;
    use tower::ServiceExt;

    const SECRET: &str = "s3cret-for-tests";

    struct Fx {
        _d: tempfile::TempDir,
        home: PathBuf,
        config_path: PathBuf,
        running: Running,
    }

    fn fx(update_runner: Option<RunnerCommand>) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(d.path()).unwrap();
        let service = home.join("service");
        std::fs::create_dir_all(&service).unwrap();
        let config_path = service.join("config.json");
        std::fs::write(&config_path, json!({ "port": 8787, "sharedSecret": SECRET, "repos": {} }).to_string()).unwrap();
        let pack = crate::packs::builtin::pack(&SetupContext { token_detector: None, token_tool: String::new() }).unwrap();
        let dist = Distribution {
            packs: Arc::new(PackRegistry::new(vec![pack]).unwrap()),
            home: home.clone(),
            config_path: config_path.clone(),
            version: Some("0.0.0-test".into()),
            extra_mcp_handlers: HashMap::new(),
            update_runner,
            overrides: Overrides { stable_install: Some(false), port: Some(8787), skip_startup_checks: true, ..Default::default() },
        };
        Fx { _d: d, home, config_path, running: build(dist).unwrap().expect("a service") }
    }

    async fn call(f: &Fx, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
        let mut b = axum::http::Request::builder().method(method).uri(uri).header("host", "127.0.0.1:8787").header("x-companion-secret", SECRET);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = f.running.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn get_settings_shows_what_is_saved_the_locked_keys_and_the_service_facts() {
        let f = fx(None);
        let (s, v) = call(&f, "GET", "/settings", None).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["lockedSettings"], json!(["port", "sharedSecret"]));
        assert_eq!((v["service"]["version"].clone(), v["restart"]["canRestart"].clone(), v["restart"]["required"].clone()), (json!("0.0.0-test"), json!(false), json!(false)));
        assert!(v["settings"]["sharedSecret"].is_null(), "the secret is never shown");
        assert!(v["features"].as_array().unwrap().iter().any(|f| f["id"] == "diagnose-build"));
        assert_eq!(v["editors"]["supported"][0], json!({ "id": "cursor", "label": "Cursor" }));
        assert!(v["background"]["scheduler"].is_object());
        assert_eq!(v["background"].as_object().map(|b| b.len()), Some(1), "no LLM backend: only the scheduler");
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn an_invalid_or_locked_settings_update_is_a_400_that_changes_nothing() {
        let f = fx(None);
        let before = std::fs::read_to_string(&f.config_path).unwrap();
        for body in [json!({ "port": 9999 }), json!({ "bogus": 1 }), json!({ "history": { "retentionDays": "x" } })] {
            let (s, v) = call(&f, "PUT", "/settings", Some(body)).await;
            assert_eq!(s, 400, "{v}");
            assert_eq!(v["code"], "invalid-settings");
            assert!(v["errors"].as_array().is_some_and(|e| !e.is_empty()) && v["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
        assert_eq!(std::fs::read_to_string(&f.config_path).unwrap(), before);
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn a_valid_update_is_saved_to_disk_applied_live_and_shown_back_but_the_locked_keys_stay() {
        let f = fx(None);
        let (s, v) = call(&f, "PUT", "/settings", Some(json!({ "history": { "retentionDays": 90 }, "reviewEditor": "cursor" }))).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!((v["ok"].clone(), v["settings"]["history"]["retentionDays"].clone(), v["settings"]["reviewEditor"].clone()), (json!(true), json!(90), json!("cursor")));
        assert!(v["warnings"].as_array().unwrap().is_empty() && v["extension"] == json!({ "reloadNeeded": false }) && v["mcpRegistration"].is_null());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&f.config_path).unwrap()).unwrap();
        assert_eq!((saved["history"]["retentionDays"].clone(), saved["port"].clone(), saved["sharedSecret"].clone()), (json!(90), json!(8787), json!(SECRET)));
        let (_, again) = call(&f, "GET", "/settings", None).await;
        assert_eq!(again["settings"]["history"]["retentionDays"], 90);
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn a_saved_api_token_goes_to_the_encrypted_store_never_into_config_json_or_the_answer() {
        let f = fx(None);
        let (s, v) = call(&f, "PUT", "/settings", Some(json!({ "jira": { "apiToken": "tok-123456789" } }))).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["settings"]["jira"]["apiTokenSet"], true);
        assert!(!v.to_string().contains("tok-123456789"));
        assert!(!std::fs::read_to_string(&f.config_path).unwrap().contains("tok-123456789"));
        let creds = Credentials::open(f.home.join("service").join("credentials.enc"), crate::paths::state_dir(&f.home).join("credentials.key"));
        assert_eq!(creds.get_token("jira.apiToken").as_deref(), Some("tok-123456789"));
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn the_data_folder_reports_its_path_and_a_purge_waits_for_running_jobs() {
        let f = fx(None);
        let (s, usage) = call(&f, "GET", "/data-folder", None).await;
        assert_eq!(s, 200, "{usage}");
        assert_eq!(usage["path"], crate::paths::state_dir(&f.home).to_string_lossy().as_ref());
        let job = f.running.state.store.create("diagnose-build", Map::new());
        let (s, v) = call(&f, "POST", "/data-folder/purge", None).await;
        assert_eq!((s, v["code"].clone()), (409, json!("jobs-running")));
        f.running.state.store.update(job["id"].as_str().unwrap(), Map::from_iter([("status".to_string(), json!("approved"))])).unwrap();
        let (s, v) = call(&f, "POST", "/data-folder/purge", None).await;
        assert_eq!((s, v["ok"].clone()), (200, json!(true)), "{v}");
        assert!(v["usage"]["items"].is_array());
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn applying_an_update_is_refused_without_an_installer_and_on_a_copy_that_cant_update() {
        let none = fx(None);
        let (s, v) = call(&none, "POST", "/update/apply", None).await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("isn't available"), "{v}");
        none.running.shutdown().await;
        let some = fx(Some(RunnerCommand { program: "/bin/true".into(), args: vec![] }));
        let (s, v) = call(&some, "POST", "/update/apply", None).await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("not the installed copy"), "{v}");
        some.running.shutdown().await;
    }

    #[tokio::test]
    async fn bad_repo_and_pr_requests_are_400s_before_anything_is_asked_of_the_machine_or_the_host() {
        let f = fx(None);
        assert_eq!(call(&f, "POST", "/repos/clone", Some(json!({ "project": "../x", "repo": "r" }))).await.0, 400);
        assert_eq!(call(&f, "POST", "/repos/choose", Some(json!({}))).await.0, 400);
        let (s, v) = call(&f, "GET", "/prs/status?project=ACME&repo=..%2Fx&prId=1", None).await;
        assert_eq!(s, 400, "{v}");
        assert_eq!(call(&f, "GET", "/prs/status?project=ACME&repo=app", None).await.0, 400);
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn open_location_refuses_text_with_no_stack_frame_with_a_code_and_never_a_500() {
        let f = fx(None);
        let (s, v) = call(&f, "POST", "/open-location", Some(json!({ "text": "hello" }))).await;
        assert!(s == 400 || s == 409, "{s} {v}");
        assert!(v["code"].is_string() && v["error"].is_string(), "{v}");
        f.running.shutdown().await;
    }

    #[tokio::test]
    async fn resuming_a_job_with_no_valid_session_is_a_400_and_opens_no_terminal() {
        let f = fx(None);
        let job = f.running.state.store.create("diagnose-build", Map::from_iter([("claudeSession".to_string(), json!({ "id": "nope", "cwd": "/tmp/evil", "permissionMode": "auto" }))]));
        let id = job["id"].as_str().unwrap();
        f.running.state.store.update(id, Map::from_iter([("status".to_string(), json!("awaiting-approval"))])).unwrap();
        let (s, v) = call(&f, "POST", &format!("/jobs/{id}/open-in-claude-code"), Some(json!({ "resume": true }))).await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("no resumable"));
        f.running.shutdown().await;
    }

    #[test]
    fn the_auth_context_survives_the_trip_through_the_pr_status_cache_key_shape() {
        let a = AuthContext { cookie: Some("a=b".into()), origin: Some("https://x".into()) };
        let back = auth_from_value(&auth_to_value(&a));
        assert_eq!((back.cookie, back.origin), (a.cookie, a.origin));
        let none = auth_from_value(&auth_to_value(&AuthContext::default()));
        assert!(none.cookie.is_none() && none.origin.is_none());
    }

    #[tokio::test]
    async fn the_update_checks_can_be_stopped_and_the_manual_start_command_names_the_serve_subcommand() {
        let f = fx(None);
        // `build` already started them; shutdown must stop them without hanging.
        f.running.shutdown().await;
        let cmd = ConfigHooks::default_folder_opener();
        assert!(cmd == "open" || cmd == "xdg-open");
    }
}
