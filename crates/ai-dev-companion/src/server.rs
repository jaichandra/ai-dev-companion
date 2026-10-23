//! The HTTP service (port of `server.ts`): every route the Chrome extension, setup and doctor
//! talk to, as an axum [`Router`]. This module is the web layer only: the Host-header check, the
//! shared-secret check and CORS, the feature routes (start / approve / reject / cancel /
//! post-comment / actions / refresh-diff), job status, pending-start jobs, the inbox, the
//! extension hello and session hosts, and the thin routes whose work lives elsewhere (settings,
//! update, data folder, open-location, PR status, repo setup). Everything that is not plain HTTP
//! sits behind [`AppHooks`], so the routes are tested with fakes and the real wiring (config,
//! providers, packs, history, updater) is `main`'s job.
//!
//! Order matters, as in Node: the Host check wraps everything; `/health` and `/mcp` come before
//! the shared-secret check (they have their own gate / need none); everything else is behind it.

// A route helper's `Err` is the ready-made error response: that is the point, not a cost.
#![allow(clippy::result_large_err)]

use crate::addons::JobAddon;
use crate::atlassian::AuthContext;
use crate::background_routes::BackgroundRoutes;
use crate::background_wiring::{feature_context_for, StartVia};
use crate::config::ConfigFn;
use crate::host_guard::check_host;
use crate::jobs::{Feature, FeatureContext, FeatureError, JobStore};
use crate::scope_key::is_valid_scope_key;
use crate::terminal::{HeaderSegment, HeaderStyle};
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MCP_BODY_LIMIT: usize = 4 * 1024 * 1024;
const RESUME_HEADER_RULE_LEN: usize = 64;

/// An error answered as `status` with `body`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub body: Value,
}

impl ApiError {
    pub fn new(status: u16, message: &str) -> Self {
        Self { status, body: json!({ "error": message }) }
    }

    pub fn with_body(status: u16, body: Value) -> Self {
        Self { status, body }
    }
}

pub type HookResult = Result<Value, ApiError>;

/// How the service answers a request to resume a job's Claude session.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeResult {
    /// With where its terminal opened (`TerminalLocation::to_json`), when known.
    Resumed(Option<Value>),
    /// Not a session this service would have recorded.
    Invalid,
    /// Its folder or transcript is gone.
    Gone,
}

/// `/mcp`: its own trust boundary (a separate bearer token), so the whole request goes to the
/// handler, which answers every method itself (GET and DELETE are "not allowed" there).
#[async_trait]
pub trait McpHandler: Send + Sync {
    /// `(status, body)`; a `None` body is an empty answer.
    async fn handle(&self, method: &str, host: Option<&str>, authorization: Option<&str>, body: &[u8]) -> (u16, Option<Value>);
}

fn unavailable<T>(what: &str) -> Result<T, ApiError> {
    Err(ApiError::new(501, &format!("{what} isn't available in this build.")))
}

/// Everything the routes need that isn't plain HTTP. The defaults answer 501 (or "nothing") so a
/// test or a distribution only supplies what it has.
#[async_trait]
pub trait AppHooks: Send + Sync {
    /// The live shared secret (`config.sharedSecret`).
    fn shared_secret(&self) -> String;
    /// The installed version, if known.
    fn version(&self) -> Option<String> {
        None
    }

    /// Fails with a user-facing 400 when a prerequisite of the feature is failing right now.
    fn assert_feature_ready(&self, _feature_id: &str, _label: &str) -> Result<(), ApiError> {
        Ok(())
    }
    /// Resolves the PR's clone before a repo-based feature starts, so a missing clone surfaces as
    /// the 409 `repo-not-found` the extension answers by offering to pick or clone it.
    fn assert_repo_available(&self, _feature_id: &str, _payload: &Value) -> Result<(), ApiError> {
        Ok(())
    }
    /// Caches whatever session this request relayed (see the auth-context module).
    fn record_session(&self, _auth: &AuthContext) {}
    fn scope_key_for(&self, _feature_id: &str, _payload: &Value) -> Option<String> {
        None
    }

    async fn data_folder(&self) -> HookResult {
        unavailable("The data folder")
    }
    async fn data_folder_open(&self) -> HookResult {
        unavailable("Opening the data folder")
    }
    async fn data_folder_purge(&self) -> HookResult {
        unavailable("Purging the data folder")
    }
    async fn open_location(&self, _body: &Value) -> HookResult {
        unavailable("Open in editor")
    }
    /// `None`: the local history is off.
    fn history_metrics(&self, _days: i64) -> Option<HookResult> {
        None
    }
    fn targets(&self) -> Value {
        json!({})
    }
    async fn pr_status(&self, _auth: &AuthContext, _project: &str, _repo: &str, _pr_id: Option<i64>) -> HookResult {
        unavailable("Pull request status")
    }
    /// `completedAt` of the saved analysis for the ticket; `None` if none (or not an issue key).
    fn analysis_cache(&self, _issue_key: &str) -> Option<String> {
        None
    }
    /// `(completedAt, commentCount)` of the saved summary.
    fn summary_cache(&self, _issue_key: &str) -> Option<(String, Value)> {
        None
    }
    /// `{ heartbeat, ... }` for `/extension/session-hosts`; the hello answers with its
    /// `heartbeat` member.
    fn heartbeat_hosts(&self) -> Value {
        json!({ "heartbeat": false, "hosts": [] })
    }
    fn configured_origins(&self) -> Vec<String> {
        vec![]
    }
    async fn update_info(&self, _refresh: bool) -> HookResult {
        unavailable("Update checks")
    }
    fn update_apply(&self) -> HookResult {
        unavailable("Updating")
    }
    async fn settings_get(&self) -> HookResult {
        unavailable("Settings")
    }
    async fn settings_put(&self, _body: &Value) -> HookResult {
        unavailable("Settings")
    }
    async fn repos_choose(&self, _body: &Value) -> HookResult {
        unavailable("Choosing a repository folder")
    }
    fn repos_clone(&self, _body: &Value, _origin: Option<&str>) -> HookResult {
        unavailable("Cloning")
    }
    /// Whether the service runs under launchd (so it can restart itself by stopping).
    fn under_launchd(&self) -> bool {
        false
    }
    fn manual_start_command(&self) -> String {
        "npm start".into()
    }
    fn started_at(&self) -> String {
        String::new()
    }
    /// Stops the process so launchd starts it again. Called a moment after the answer is sent.
    fn restart_process(&self) {}
    /// Opens a terminal running `claude` in `dir`, titled `title`; where it opened, when known.
    async fn open_claude_code(&self, _dir: &str, _title: Option<String>) -> Result<Option<Value>, String> {
        Err("Opening Claude Code isn't available in this build.".into())
    }
    /// Brings the terminal at a stored location to the front: `Ok(false)` if it has been closed.
    async fn focus_terminal(&self, _location: &Value) -> Result<bool, String> {
        Err("Focusing a terminal isn't available in this build.".into())
    }
    async fn resume_session(&self, _session: &Value, _header: &[Vec<HeaderSegment>], _job_worktree: Option<&str>) -> Result<ResumeResult, String> {
        Err("Resuming a session isn't available in this build.".into())
    }
}

pub struct AppState {
    pub port: u16,
    pub store: Arc<JobStore>,
    pub features: Vec<Arc<dyn Feature>>,
    pub hooks: Arc<dyn AppHooks>,
    /// The inbox routes; `None` answers 404.
    pub background: Option<Arc<BackgroundRoutes>>,
    /// `/mcp`; `None` answers 404.
    pub mcp: Option<Arc<dyn McpHandler>>,
    pub addons: Vec<Arc<dyn JobAddon>>,
    config: Option<ConfigFn>,
    extension_hello: Mutex<Option<(Option<String>, String)>>,
    jobs_being_started: Mutex<HashSet<String>>,
}

impl AppState {
    pub fn new(port: u16, store: Arc<JobStore>, features: Vec<Arc<dyn Feature>>, hooks: Arc<dyn AppHooks>) -> Self {
        Self { port, store, features, hooks, background: None, mcp: None, addons: vec![], config: None, extension_hello: Mutex::new(None), jobs_being_started: Mutex::new(HashSet::new()) }
    }

    pub fn with_background(mut self, background: Arc<BackgroundRoutes>) -> Self {
        self.background = Some(background);
        self
    }

    pub fn with_mcp(mut self, mcp: Arc<dyn McpHandler>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    pub fn with_addons(mut self, addons: Vec<Arc<dyn JobAddon>>, config: ConfigFn) -> Self {
        self.addons = addons;
        self.config = Some(config);
        self
    }

    fn config_value(&self) -> Value {
        self.config.as_ref().map(|c| c()).unwrap_or_else(|| json!({}))
    }

    fn feature(&self, id: &str) -> Option<&Arc<dyn Feature>> {
        self.features.iter().find(|f| f.id() == id)
    }

    /// The job as the page sees it: a feature may re-check what it hands out.
    fn present(&self, job: Value) -> Value {
        match job["featureId"].as_str().and_then(|id| self.feature(id)) {
            Some(f) => f.present(job),
            None => job,
        }
    }
}

type Shared = Arc<AppState>;

/// A start that didn't happen: our own check, or the feature's refusal.
#[derive(Debug)]
pub enum StartError {
    Api(ApiError),
    Feature(FeatureError),
}

/// The one place a feature starts, whether the click came from the extension, an MCP tool or a
/// watcher, or `POST /jobs/:jobId/start` turning a "pending-start" job into a real one. Runs the
/// readiness and repo checks, then tags the job with its scope key and how it was started.
pub async fn start_feature_job(state: &AppState, feature: &Arc<dyn Feature>, payload: Value, ctx: FeatureContext, via: StartVia, watcher: Option<&str>) -> Result<Value, StartError> {
    state.hooks.assert_feature_ready(feature.id(), feature.label()).map_err(StartError::Api)?;
    state.hooks.assert_repo_available(feature.id(), &payload).map_err(StartError::Api)?;
    let job = feature.start(payload.clone(), feature_context_for(via, &ctx)).await.map_err(StartError::Feature)?;
    let mut patch = Map::new();
    if let Some(k) = state.hooks.scope_key_for(feature.id(), &payload) {
        patch.insert("scopeKey".into(), json!(k));
    }
    patch.insert("startedVia".into(), json!(via.as_str()));
    if let Some(w) = watcher {
        patch.insert("watcher".into(), json!(w));
    }
    let id = job["id"].as_str().unwrap_or("").to_string();
    Ok(state.store.update(&id, patch).unwrap_or(job))
}

fn reply(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(body)).into_response()
}

fn api_reply(e: ApiError) -> Response {
    reply(e.status, e.body)
}

fn hook_reply(r: HookResult) -> Response {
    match r {
        Ok(v) => reply(200, v),
        Err(e) => api_reply(e),
    }
}

fn feature_error_reply(e: &FeatureError) -> Response {
    match e {
        FeatureError::StartRefused { message, code, details } => {
            let mut body = details.clone();
            body.insert("error".into(), json!(message));
            body.insert("code".into(), json!(code));
            reply(409, Value::Object(body))
        }
        FeatureError::JobStatus(m) => reply(409, json!({ "error": m })),
        FeatureError::WorktreeGone(m) => reply(410, json!({ "error": m })),
        FeatureError::Other(m) => reply(500, json!({ "error": m })),
    }
}

fn start_error_reply(e: StartError) -> Response {
    match e {
        StartError::Api(a) => api_reply(a),
        StartError::Feature(f) => feature_error_reply(&f),
    }
}

/// An empty body is `{}` (as Express leaves `req.body`); anything else must be JSON.
fn body_of(bytes: &Bytes) -> Result<Value, Response> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(bytes).map_err(|_| reply(400, json!({ "error": "The request body isn't valid JSON." })))
}

/// The page's browser session, relayed from the extension, plus the request body.
fn context_for(state: &AppState, headers: &HeaderMap, body: Value) -> FeatureContext {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(String::from);
    let auth = AuthContext { cookie: header("x-relay-cookie"), origin: header("x-relay-origin") };
    state.hooks.record_session(&auth);
    FeatureContext { auth, body: Some(body), background: false }
}

fn unknown_job() -> Response {
    reply(404, json!({ "error": "unknown job id" }))
}

fn host_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

// ---- Middleware ----

async fn host_guard(State(state): State<Shared>, req: Request, next: Next) -> Response {
    match check_host(host_header(req.headers()), state.port) {
        Ok(()) => next.run(req).await,
        Err(body) => reply(403, body),
    }
}

/// The secret check, constant-time. No CORS headers are sent: the extension's service worker is
/// the only caller that needs this API, and Chrome exempts its fetches to a manifest host from
/// CORS (and sends no Origin), so a web page gets no preflight answer and can't read a reply.
/// Deliberately not an Origin check, for that same reason.
async fn secret_guard(State(state): State<Shared>, req: Request, next: Next) -> Response {
    let given = req.headers().get("x-companion-secret").map(|v| v.as_bytes());
    let expected = state.hooks.shared_secret();
    if !secret_matches(given, expected.as_bytes()) {
        return reply(403, json!({ "error": "The Chrome extension and the companion service aren't using the same secret. Reload the extension at chrome://extensions (setup regenerates its config); if that doesn't help, re-run `npm run setup` in the companion-service folder, then reload the extension again." }));
    }
    next.run(req).await
}

/// Whether `given` is the (non-empty) `expected` secret, compared in constant time once the
/// lengths agree.
fn secret_matches(given: Option<&[u8]>, expected: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    match given {
        Some(g) if !expected.is_empty() => g.len() == expected.len() && bool::from(g.ct_eq(expected)),
        _ => false,
    }
}

// ---- Routes: open (no secret) ----

async fn health(State(state): State<Shared>) -> Response {
    reply(200, json!({ "ok": true, "version": state.hooks.version(), "features": state.features.iter().map(|f| f.id()).collect::<Vec<_>>() }))
}

async fn mcp(State(state): State<Shared>, method: Method, headers: HeaderMap, body: Bytes) -> Response {
    let Some(handler) = &state.mcp else { return reply(404, json!({ "error": "not found" })) };
    if body.len() > MCP_BODY_LIMIT {
        return reply(413, json!({ "jsonrpc": "2.0", "error": { "code": -32600, "message": "Request body too large" }, "id": null }));
    }
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let (status, out) = handler.handle(method.as_str(), host_header(&headers), authorization, &body).await;
    match out {
        Some(v) => reply(status, v),
        None => StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

// ---- Routes: behind the secret ----

async fn data_folder(State(state): State<Shared>) -> Response {
    hook_reply(state.hooks.data_folder().await)
}

async fn data_folder_open(State(state): State<Shared>) -> Response {
    hook_reply(state.hooks.data_folder_open().await)
}

async fn data_folder_purge(State(state): State<Shared>) -> Response {
    // A running job is using its worktree; a restart-safe purge waits for it.
    if state.store.active_count() > 0 {
        return reply(409, json!({ "code": "jobs-running", "error": "Jobs are still running. Wait for them to finish (or cancel them), then purge." }));
    }
    hook_reply(state.hooks.data_folder_purge().await)
}

async fn open_location(State(state): State<Shared>, body: Bytes) -> Response {
    match body_of(&body) {
        Ok(b) => hook_reply(state.hooks.open_location(&b).await),
        Err(r) => r,
    }
}

async fn history_metrics(State(state): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let days = match q.get("days") {
        None => 30,
        Some(d) => match d.trim().parse::<f64>() {
            Ok(n) if n.fract() == 0.0 && (1.0..=3650.0).contains(&n) => n as i64,
            _ => return reply(400, json!({ "error": "days must be a whole number from 1 to 3650." })),
        },
    };
    match state.hooks.history_metrics(days) {
        None => reply(404, json!({ "error": "Local history is off on this copy of the companion." })),
        Some(r) => hook_reply(r),
    }
}

async fn targets(State(state): State<Shared>) -> Response {
    reply(200, state.hooks.targets())
}

async fn pr_status(State(state): State<Shared>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let ctx = context_for(&state, &headers, json!({}));
    let text = |k: &str| q.get(k).cloned().unwrap_or_default();
    let pr_id = text("prId").trim().parse::<f64>().ok().filter(|n| n.fract() == 0.0).map(|n| n as i64);
    hook_reply(state.hooks.pr_status(&ctx.auth, &text("project"), &text("repo"), pr_id).await)
}

async fn features_list(State(state): State<Shared>) -> Response {
    reply(200, Value::Array(state.features.iter().map(|f| json!({ "id": f.id(), "label": f.label() })).collect()))
}

async fn analysis_cache(State(state): State<Shared>, Path(key): Path<String>) -> Response {
    let at = state.hooks.analysis_cache(&key);
    reply(200, json!({ "exists": at.is_some(), "completedAt": at }))
}

async fn summary_cache(State(state): State<Shared>, Path(key): Path<String>) -> Response {
    match state.hooks.summary_cache(&key) {
        // commentCount lets the extension tell a current summary from a stale one.
        Some((at, count)) => reply(200, json!({ "exists": true, "completedAt": at, "commentCount": count })),
        None => reply(200, json!({ "exists": false, "completedAt": null, "commentCount": null })),
    }
}

/// The extension says hello whenever it's installed, reloaded or Chrome starts; setup polls the
/// GET to know Chrome is connected. In memory only: a restart forgets it.
async fn extension_hello_post(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let version = body["version"].as_str().map(String::from);
    let at = crate::js_date::to_iso_string(crate::job_files::now_ms() as i64).unwrap_or_default();
    *state.extension_hello.lock().unwrap() = Some((version, at));
    // The hello carries the same relay headers as any other request, so it warms the session
    // vault before any feature has run.
    context_for(&state, &headers, body);
    reply(200, json!({ "ok": true, "heartbeat": state.hooks.heartbeat_hosts()["heartbeat"] }))
}

async fn extension_hello_get(State(state): State<Shared>) -> Response {
    match &*state.extension_hello.lock().unwrap() {
        Some((version, at)) => reply(200, json!({ "connected": true, "version": version, "at": at })),
        None => reply(200, json!({ "connected": false })),
    }
}

async fn session_hosts(State(state): State<Shared>) -> Response {
    reply(200, state.hooks.heartbeat_hosts())
}

async fn session_origins(State(state): State<Shared>) -> Response {
    reply(200, json!({ "hosts": state.hooks.configured_origins() }))
}

async fn update_info(State(state): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    match state.hooks.update_info(q.get("refresh").map(String::as_str) == Some("1")).await {
        Ok(v) => reply(200, v),
        Err(e) => reply(500, e.body),
    }
}

async fn update_apply(State(state): State<Shared>) -> Response {
    match state.hooks.update_apply() {
        Ok(v) => reply(200, v),
        Err(e) => reply(400, e.body),
    }
}

async fn settings_get(State(state): State<Shared>) -> Response {
    hook_reply(state.hooks.settings_get().await)
}

async fn settings_put(State(state): State<Shared>, body: Bytes) -> Response {
    match body_of(&body) {
        Ok(b) => hook_reply(state.hooks.settings_put(&b).await),
        Err(r) => r,
    }
}

async fn settings_restart(State(state): State<Shared>, body: Bytes) -> Response {
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if !state.hooks.under_launchd() {
        return reply(400, json!({ "error": format!("The companion service isn't running in the background, so it can't restart itself. Stop it (Ctrl+C in its terminal) and start it again: {}", state.hooks.manual_start_command()) }));
    }
    let active = state.store.active_count();
    if active > 0 && body["force"] != json!(true) {
        return reply(409, json!({ "error": format!("{active} job(s) are still running \u{2014} restarting now would stop them."), "code": "jobs-running", "activeJobs": active }));
    }
    let hooks = state.hooks.clone();
    // Not an exit(0): the launchd job's KeepAlive only restarts a job that ends unsuccessfully,
    // and dying from a signal counts.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        hooks.restart_process();
    });
    reply(200, json!({ "ok": true, "startedAt": state.hooks.started_at() }))
}

async fn job_status(State(state): State<Shared>, Path(job_id): Path<String>) -> Response {
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let feature = job["featureId"].as_str().and_then(|id| state.feature(id)).cloned();
    let presented = state.present(job.clone());
    // A feature that hands a job off (review-in-editor's terminal) reports its live status too.
    match feature {
        Some(f) => match f.session_status(&job).await {
            Some(status) => reply(200, with_session(presented, Some(status))),
            None => reply(200, presented),
        },
        None => reply(200, presented),
    }
}

fn resume_header_key(job: &Value) -> Option<String> {
    let pr_id = &job["data"]["payload"]["prId"];
    if !pr_id.is_null() {
        return Some(format!("PR #{}", crate::js_text::js_to_string(pr_id)));
    }
    job["data"]["issueKey"].as_str().filter(|k| !k.is_empty()).map(String::from)
}

/// The 410 body for an expired Claude Code session, worded per feature: each points at the button
/// that starts a fresh one.
fn expired_session_message(feature_id: &str) -> String {
    let prefix = "This Claude Code session has expired (its working copy or saved transcript is gone)";
    match feature_id {
        "analyze-issue" => format!("{prefix} \u{2014} click Re-analyze to start a new session."),
        "resolve-conflict" => format!("{prefix} \u{2014} use Open in Claude Code to start a fresh one in this worktree."),
        _ => format!("{prefix}."),
    }
}

fn build_resume_header(label: &str, job: &Value) -> Vec<Vec<HeaderSegment>> {
    let rule = "\u{2500}".repeat(RESUME_HEADER_RULE_LEN);
    let mut title = vec![HeaderSegment::styled(label, HeaderStyle::Title)];
    if let Some(k) = resume_header_key(job) {
        title.push(HeaderSegment::styled(&format!("  {k}"), HeaderStyle::Heading));
    }
    vec![vec![HeaderSegment::styled(&rule, HeaderStyle::Rule)], title, vec![HeaderSegment::styled(&rule, HeaderStyle::Rule)], vec![HeaderSegment::styled("Resuming Claude Code\u{2026}", HeaderStyle::Status)], vec![]]
}

async fn addons_list(State(state): State<Shared>, Path(job_id): Path<String>) -> Response {
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let config = state.config_value();
    let mut views = vec![];
    for addon in &state.addons {
        if let Some(v) = addon.view(&job, &config).await {
            views.push(v.to_json());
        }
    }
    reply(200, json!({ "addons": views }))
}

async fn addon_op(State(state): State<Shared>, Path((job_id, addon_id, op)): Path<(String, String, String)>) -> Response {
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let Some(addon) = state.addons.iter().find(|a| a.id() == addon_id) else { return reply(404, json!({ "error": "unknown add-on" })) };
    let config = state.config_value();
    let result = match op.as_str() {
        "start" => addon.start(&job, &config).await,
        "stop" => addon.stop(&job, &config).await,
        _ => return reply(404, json!({ "error": "unknown add-on operation" })),
    };
    match result {
        Ok(view) => reply(200, json!({ "addon": view.to_json() })),
        Err(e) => reply(500, json!({ "error": e })),
    }
}

async fn addon_heartbeat(State(state): State<Shared>, body: Bytes) -> Response {
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(origin) = body["origin"].as_str() else { return reply(400, json!({ "error": "origin is required" })) };
    let alive = state.addons.iter().any(|a| a.heartbeat(origin));
    reply(if alive { 200 } else { 404 }, json!({ "ok": alive }))
}

/// Generic (not per feature): "open a terminal at this job's worktree and run claude" doesn't
/// depend on which feature created it, only on the `data.worktree.dir` convention. The path is
/// never client-supplied. A body of `{resume: true}` instead resumes the job's headless Claude
/// session, read only from the server-held job data.
async fn open_in_claude_code(State(state): State<Shared>, Path(job_id): Path<String>, body: Bytes) -> Response {
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    if body["resume"] != json!(true) {
        // A Review PR job keeps its (shared) review worktree in its result, not in data.worktree.
        let review_dir = &job["result"]["review"]["worktree"];
        let dir = job["data"]["worktree"]["dir"].as_str().or_else(|| (job["featureId"] == "review-in-editor").then(|| review_dir.as_str()).flatten());
        let Some(dir) = dir else { return reply(400, json!({ "error": "this job has no associated worktree" })) };
        if !std::path::Path::new(dir).exists() {
            return reply(410, json!({ "error": "the worktree no longer exists (the job may already have been approved or discarded)" }));
        }
        return match state.hooks.open_claude_code(dir, terminal_title(&state, &job)).await {
            Ok(location) => {
                record_terminal(&state, &job_id, location);
                reply(200, json!({ "ok": true }))
            }
            Err(e) => reply(500, json!({ "error": e })),
        };
    }
    let status = job["status"].as_str().unwrap_or("");
    if matches!(status, "running" | "approving" | "rejecting") {
        return reply(409, json!({ "error": format!("This job is still {status} \u{2014} wait for it to finish before resuming.") }));
    }
    let feature_id = job["featureId"].as_str().unwrap_or("");
    let header = state.feature(feature_id).map(|f| build_resume_header(f.label(), &job)).unwrap_or_default();
    match state.hooks.resume_session(&job["data"]["claudeSession"], &header, job["data"]["worktree"]["dir"].as_str()).await {
        Ok(ResumeResult::Resumed(location)) => {
            record_terminal(&state, &job_id, location);
            reply(200, json!({ "ok": true }))
        }
        Ok(ResumeResult::Invalid) => reply(400, json!({ "error": "This job has no resumable Claude Code session." })),
        Ok(ResumeResult::Gone) => reply(410, json!({ "error": expired_session_message(feature_id) })),
        Err(e) => reply(500, json!({ "error": e })),
    }
}

/// What the terminal's tab is called: the feature and the PR or ticket it is for, so its window
/// can be told from the others'.
fn terminal_title(state: &Shared, job: &Value) -> Option<String> {
    let label = state.feature(job["featureId"].as_str().unwrap_or("")).map(|f| f.label().to_string())?;
    Some(match resume_header_key(job) {
        Some(key) => format!("{label} {key}"),
        None => label,
    })
}

/// Remembers where a job's terminal opened (`data.terminal`) so the slideout can bring it back.
/// A launch that couldn't say where leaves any earlier location alone.
fn record_terminal(state: &Shared, job_id: &str, location: Option<Value>) {
    if let Some(location) = location {
        let _ = state.store.set_data(job_id, "terminal", location);
    }
}

/// Brings the job's terminal window to the front. `focused`, `missing` (it was closed) or
/// `unknown` (no location was recorded). The location is only ever the server-held one.
async fn focus_job_terminal(State(state): State<Shared>, Path(job_id): Path<String>) -> Response {
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let location = &job["data"]["terminal"];
    if location.is_null() {
        return reply(200, json!({ "focus": "unknown" }));
    }
    match state.hooks.focus_terminal(location).await {
        Ok(true) => reply(200, json!({ "focus": "focused" })),
        Ok(false) => reply(200, json!({ "focus": "missing" })),
        Err(e) => reply(500, json!({ "error": e })),
    }
}

async fn notifications_list(State(state): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(bg) = &state.background else { return reply(404, json!({ "error": "not found" })) };
    let (s, b) = bg.list(q.get("all").map(String::as_str) == Some("1"));
    reply(s, b)
}

async fn notifications_seen(State(state): State<Shared>, body: Bytes) -> Response {
    let Some(bg) = &state.background else { return reply(404, json!({ "error": "not found" })) };
    match body_of(&body) {
        Ok(b) => {
            let (s, v) = bg.seen(&b);
            reply(s, v)
        }
        Err(r) => r,
    }
}

async fn notifications_announce(State(state): State<Shared>) -> Response {
    let Some(bg) = &state.background else { return reply(404, json!({ "error": "not found" })) };
    let (s, b) = bg.announce();
    reply(s, b)
}

async fn notifications_open(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    let Some(bg) = &state.background else { return reply(404, json!({ "error": "not found" })) };
    let (s, b) = bg.open(&id);
    reply(s, b)
}

async fn repos_choose(State(state): State<Shared>, body: Bytes) -> Response {
    match body_of(&body) {
        Ok(b) => hook_reply(state.hooks.repos_choose(&b).await),
        Err(r) => r,
    }
}

async fn repos_clone(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let origin = headers.get("x-relay-origin").and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty());
    match body_of(&body) {
        Ok(b) => hook_reply(state.hooks.repos_clone(&b, origin)),
        Err(r) => r,
    }
}

// ---- Feature routes ----

fn feature_or_404(state: &AppState, id: &str) -> Result<Arc<dyn Feature>, Response> {
    state.feature(id).cloned().ok_or_else(|| reply(404, json!({ "error": "not found" })))
}

async fn feature_start(State(state): State<Shared>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let feature = match feature_or_404(&state, &id) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ctx = context_for(&state, &headers, body.clone());
    match start_feature_job(&state, &feature, body, ctx, StartVia::Extension, None).await {
        Ok(job) => reply(200, state.present(job)),
        Err(e) => start_error_reply(e),
    }
}

enum Op {
    Approve,
    Reject,
}

async fn feature_approve_reject(state: Shared, id: String, job_id: String, headers: HeaderMap, body: Bytes, op: Op) -> Response {
    let feature = match feature_or_404(&state, &id) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let ctx = context_for(&state, &headers, body);
    let result = match op {
        Op::Approve => feature.approve(job.clone(), ctx).await,
        Op::Reject => feature.reject(job.clone(), ctx).await,
    };
    match result {
        // The job as it was handed to the feature, as Express answers with the same object the
        // feature may have mutated in place.
        Ok(()) => reply(200, state.present(state.store.get(&job_id).unwrap_or(job))),
        Err(e) => feature_error_reply(&e),
    }
}

async fn feature_approve(State(state): State<Shared>, Path((id, job_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    feature_approve_reject(state, id, job_id, headers, body, Op::Approve).await
}

async fn feature_reject(State(state): State<Shared>, Path((id, job_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    feature_approve_reject(state, id, job_id, headers, body, Op::Reject).await
}

fn with_session(mut job: Value, status: Option<crate::jobs::SessionStatus>) -> Value {
    job["session"] = serde_json::to_value(status).unwrap_or(Value::Null);
    job
}

async fn feature_cancel(State(state): State<Shared>, Path((id, job_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let feature = match feature_or_404(&state, &id) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let ctx = context_for(&state, &headers, body);
    let status = job["status"].as_str().unwrap_or("").to_string();
    // The job may have finished between the click and this request; cancelling then means
    // discarding its result, i.e. a reject.
    let result: Result<Option<Value>, FeatureError> = async {
        if status == "running" && feature.supports_cancel() {
            feature.cancel(job.clone(), ctx).await?;
        } else if status == "approved" {
            feature.stop_session(job.clone()).await?;
            let session = feature.session_status(&job).await;
            return Ok(Some(with_session(job.clone(), session)));
        } else if feature.supports_cancel() && (status == "awaiting-approval" || status == "failed") {
            feature.reject(job.clone(), ctx).await?;
        }
        Ok(None)
    }
    .await;
    match result {
        Ok(Some(v)) => reply(200, v),
        Ok(None) => reply(200, state.store.get(&job_id).unwrap_or(job)),
        Err(e) => feature_error_reply(&e),
    }
}

async fn feature_post_comment(State(state): State<Shared>, Path((id, job_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let feature = match feature_or_404(&state, &id) {
        Ok(f) if f.supports_post_comment() => f,
        Ok(_) => return reply(404, json!({ "error": "not found" })),
        Err(r) => return r,
    };
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let ctx = context_for(&state, &headers, body);
    match feature.post_comment(job, ctx).await {
        Ok(comment_id) => reply(200, json!({ "ok": true, "commentId": comment_id })),
        Err(e) => reply(500, json!({ "error": e.to_string() })),
    }
}

async fn feature_action(State(state): State<Shared>, Path((id, job_id, action)): Path<(String, String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let feature = match feature_or_404(&state, &id) {
        Ok(f) if !f.action_names().is_empty() => f,
        Ok(_) => return reply(404, json!({ "error": "not found" })),
        Err(r) => return r,
    };
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ctx = context_for(&state, &headers, body);
    let present = |j: Value| state.present(j);
    match crate::feature_actions::feature_action(&state.store, feature.as_ref(), &job_id, &action, ctx, present).await {
        Ok(v) => reply(200, v),
        Err(crate::feature_actions::ActionError::NotFound(m)) => reply(404, json!({ "error": m })),
        Err(crate::feature_actions::ActionError::Feature(e)) => feature_error_reply(&e),
    }
}

async fn feature_refresh_diff(State(state): State<Shared>, Path((id, job_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let feature = match feature_or_404(&state, &id) {
        Ok(f) if f.supports_refresh_diff() => f,
        Ok(_) => return reply(404, json!({ "error": "not found" })),
        Err(r) => return r,
    };
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let ctx = context_for(&state, &headers, body);
    match feature.refresh_diff(job, ctx).await {
        Ok(v) => reply(200, v),
        Err(e) => feature_error_reply(&e),
    }
}

// ---- Jobs: lookup and pending-start ----

/// Whether a page already has a job before offering to create one via an MCP tool (read-only).
/// `featureId` must name a running feature, so this can't probe for features that aren't enabled.
async fn jobs_lookup(State(state): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let feature = q.get("featureId").and_then(|id| state.feature(id));
    match (q.get("scopeKey"), feature) {
        (Some(key), Some(f)) if is_valid_scope_key(key) => {
            let found = state.store.lookup(key, f.id());
            reply(200, json!({ "job": found.map(|j| state.present(j)) }))
        }
        _ => reply(400, json!({ "error": "scopeKey and featureId (a running feature's id) are required" })),
    }
}

/// Turns a "pending-start" job (created by an MCP tool) into a real one, only ever from a browser
/// click: a tool that would give Claude Bash access can't run without it.
async fn job_start(State(state): State<Shared>, Path(job_id): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match body_of(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let status = job["status"].as_str().unwrap_or("");
    if status != "pending-start" {
        return reply(409, json!({ "error": format!("This job is \"{status}\", not waiting to be started.") }));
    }
    if state.jobs_being_started.lock().unwrap().contains(&job_id) {
        return reply(409, json!({ "error": "This job is already being started." }));
    }
    let feature_id = job["featureId"].as_str().unwrap_or("").to_string();
    let Some(feature) = state.feature(&feature_id).cloned() else { return reply(400, json!({ "error": format!("Feature \"{feature_id}\" isn't enabled.") })) };
    state.jobs_being_started.lock().unwrap().insert(job_id.clone());
    let ctx = context_for(&state, &headers, body);
    let result = start_feature_job(&state, &feature, job["data"]["payload"].clone(), ctx, StartVia::Mcp, None).await;
    state.jobs_being_started.lock().unwrap().remove(&job_id);
    match result {
        Ok(new_job) => {
            // Only once the start has succeeded: a failed start leaves the pending job as it was,
            // so the user can still see and retry it.
            let _ = state.store.update(&job_id, Map::from_iter([("status".to_string(), json!("rejected"))]));
            let _ = state.store.patch_data(&job_id, Map::from_iter([("supersededBy".to_string(), new_job["id"].clone())]));
            reply(200, new_job)
        }
        Err(e) => start_error_reply(e),
    }
}

async fn job_dismiss(State(state): State<Shared>, Path(job_id): Path<String>) -> Response {
    let Some(job) = state.store.get(&job_id) else { return unknown_job() };
    let status = job["status"].as_str().unwrap_or("");
    if status != "pending-start" {
        return reply(409, json!({ "error": format!("This job is \"{status}\", not waiting to be started.") }));
    }
    let _ = state.store.update(&job_id, Map::from_iter([("status".to_string(), json!("rejected"))]));
    reply(200, json!({ "ok": true }))
}

async fn not_found() -> Response {
    reply(404, json!({ "error": "not found" }))
}

/// The router for the whole service.
pub fn app(state: Arc<AppState>) -> Router {
    let secured = Router::new()
        .route("/data-folder", get(data_folder))
        .route("/data-folder/open", post(data_folder_open))
        .route("/data-folder/purge", post(data_folder_purge))
        .route("/open-location", post(open_location))
        .route("/history/metrics", get(history_metrics))
        .route("/targets", get(targets))
        .route("/prs/status", get(pr_status))
        .route("/features", get(features_list))
        .route("/analysis-cache/{issue_key}", get(analysis_cache))
        .route("/summary-cache/{issue_key}", get(summary_cache))
        .route("/extension/hello", post(extension_hello_post).get(extension_hello_get))
        .route("/extension/session-hosts", get(session_hosts))
        .route("/extension/session-origins", get(session_origins))
        .route("/update", get(update_info))
        .route("/update/apply", post(update_apply))
        .route("/settings", get(settings_get).put(settings_put))
        .route("/settings/restart", post(settings_restart))
        .route("/status/{job_id}", get(job_status))
        .route("/jobs/{job_id}/open-in-claude-code", post(open_in_claude_code))
        .route("/jobs/{job_id}/focus-terminal", post(focus_job_terminal))
        .route("/jobs/{job_id}/addons", get(addons_list))
        .route("/jobs/{job_id}/addons/{addon}/{op}", post(addon_op))
        .route("/addons/heartbeat", post(addon_heartbeat))
        .route("/notifications", get(notifications_list))
        .route("/notifications/seen", post(notifications_seen))
        .route("/notifications/announce", post(notifications_announce))
        .route("/notifications/{id}/open", post(notifications_open))
        .route("/repos/choose", post(repos_choose))
        .route("/repos/clone", post(repos_clone))
        .route("/features/{id}/start", post(feature_start))
        .route("/features/{id}/{job_id}/approve", post(feature_approve))
        .route("/features/{id}/{job_id}/reject", post(feature_reject))
        .route("/features/{id}/{job_id}/cancel", post(feature_cancel))
        .route("/features/{id}/{job_id}/post-comment", post(feature_post_comment))
        .route("/features/{id}/{job_id}/actions/{action}", post(feature_action))
        .route("/features/{id}/{job_id}/refresh-diff", post(feature_refresh_diff))
        .route("/jobs/lookup", get(jobs_lookup))
        .route("/jobs/{job_id}/start", post(job_start))
        .route("/jobs/{job_id}/dismiss", post(job_dismiss))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), secret_guard));
    Router::new().route("/health", get(health)).route("/mcp", any(mcp)).merge(secured).layer(middleware::from_fn_with_state(state.clone(), host_guard)).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::{AddonState, AddonView};
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const SECRET: &str = "s3cret";
    const PORT: u16 = 8787;

    struct Hooks {
        calls: Mutex<Vec<String>>,
        launchd: bool,
    }

    #[async_trait]
    impl AppHooks for Hooks {
        fn shared_secret(&self) -> String {
            SECRET.into()
        }
        fn version(&self) -> Option<String> {
            Some("1.2.3".into())
        }
        fn assert_repo_available(&self, _: &str, payload: &Value) -> Result<(), ApiError> {
            if payload["project"] == "MISSING" {
                return Err(ApiError::with_body(409, json!({ "error": "no clone", "code": "repo-not-found", "project": "MISSING", "repo": "r" })));
            }
            Ok(())
        }
        fn record_session(&self, auth: &AuthContext) {
            self.calls.lock().unwrap().push(format!("session:{}", auth.origin.clone().unwrap_or_default()));
        }
        fn scope_key_for(&self, _: &str, payload: &Value) -> Option<String> {
            payload["key"].as_str().map(String::from)
        }
        fn history_metrics(&self, days: i64) -> Option<HookResult> {
            Some(Ok(json!({ "days": days })))
        }
        fn analysis_cache(&self, key: &str) -> Option<String> {
            (key == "PROJ-1").then(|| "2026-01-01T00:00:00.000Z".into())
        }
        fn heartbeat_hosts(&self) -> Value {
            json!({ "heartbeat": true, "hosts": ["https://jira.example"] })
        }
        fn under_launchd(&self) -> bool {
            self.launchd
        }
        fn restart_process(&self) {
            self.calls.lock().unwrap().push("restart".into());
        }
        async fn settings_get(&self) -> HookResult {
            Ok(json!({ "settings": {} }))
        }
        async fn open_claude_code(&self, dir: &str, title: Option<String>) -> Result<Option<Value>, String> {
            self.calls.lock().unwrap().push(format!("open:{dir}:{}", title.unwrap_or_default()));
            Ok(Some(json!({ "app": "terminal", "windowId": "7", "tty": "/dev/ttys003" })))
        }
        async fn focus_terminal(&self, location: &Value) -> Result<bool, String> {
            self.calls.lock().unwrap().push(format!("focus:{}", location["tty"].as_str().unwrap_or("")));
            Ok(location["windowId"] == "7")
        }
        async fn resume_session(&self, session: &Value, header: &[Vec<HeaderSegment>], _: Option<&str>) -> Result<ResumeResult, String> {
            self.calls.lock().unwrap().push(format!("resume:{}:{}", session["id"], header.len()));
            Ok(match session["id"].as_str() {
                Some("gone") => ResumeResult::Gone,
                Some(_) => ResumeResult::Resumed(Some(json!({ "app": "terminal", "windowId": "8", "tty": "/dev/ttys004" }))),
                None => ResumeResult::Invalid,
            })
        }
    }

    struct Fake {
        store: Arc<JobStore>,
        fail: Mutex<Option<FeatureError>>,
    }

    #[async_trait]
    impl Feature for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn label(&self) -> &str {
            "Fake feature"
        }
        async fn start(&self, payload: Value, ctx: FeatureContext) -> crate::jobs::FeatureResult<Value> {
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e);
            }
            let job = self.store.create("fake", Map::from_iter([("payload".to_string(), payload), ("background".to_string(), json!(ctx.background))]));
            Ok(job)
        }
        async fn approve(&self, job: Value, _: FeatureContext) -> crate::jobs::FeatureResult<()> {
            match self.fail.lock().unwrap().take() {
                Some(e) => Err(e),
                None => {
                    self.store.update(job["id"].as_str().unwrap(), Map::from_iter([("status".to_string(), json!("approved"))])).unwrap();
                    Ok(())
                }
            }
        }
        async fn reject(&self, job: Value, _: FeatureContext) -> crate::jobs::FeatureResult<()> {
            self.store.update(job["id"].as_str().unwrap(), Map::from_iter([("status".to_string(), json!("rejected"))])).unwrap();
            Ok(())
        }
        fn supports_cancel(&self) -> bool {
            true
        }
        async fn cancel(&self, job: Value, _: FeatureContext) -> crate::jobs::FeatureResult<()> {
            self.store.update(job["id"].as_str().unwrap(), Map::from_iter([("status".to_string(), json!("rejected"))])).unwrap();
            Ok(())
        }
        fn action_names(&self) -> Vec<&'static str> {
            vec!["ping"]
        }
        async fn run_action(&self, _: &str, _: Value, ctx: FeatureContext) -> crate::jobs::FeatureResult<Value> {
            Ok(json!({ "pong": ctx.body }))
        }
        fn present(&self, mut job: Value) -> Value {
            job["presented"] = json!(true);
            job
        }
    }

    struct Mcp;

    #[async_trait]
    impl McpHandler for Mcp {
        async fn handle(&self, method: &str, host: Option<&str>, authorization: Option<&str>, body: &[u8]) -> (u16, Option<Value>) {
            (200, Some(json!({ "method": method, "host": host, "auth": authorization, "bytes": body.len() })))
        }
    }

    struct Fx {
        app: Router,
        state: Arc<AppState>,
        hooks: Arc<Hooks>,
        fake: Arc<Fake>,
    }

    fn fx(launchd: bool) -> Fx {
        let store = Arc::new(JobStore::new());
        let hooks = Arc::new(Hooks { calls: Mutex::new(vec![]), launchd });
        let fake = Arc::new(Fake { store: store.clone(), fail: Mutex::new(None) });
        let state = Arc::new(AppState::new(PORT, store, vec![fake.clone() as Arc<dyn Feature>], hooks.clone()).with_mcp(Arc::new(Mcp)));
        Fx { app: app(state.clone()), state, hooks, fake }
    }

    fn fx_addons(addons: Vec<Arc<dyn JobAddon>>) -> Fx {
        let store = Arc::new(JobStore::new());
        let hooks = Arc::new(Hooks { calls: Mutex::new(vec![]), launchd: false });
        let fake = Arc::new(Fake { store: store.clone(), fail: Mutex::new(None) });
        let state = Arc::new(AppState::new(PORT, store, vec![fake.clone() as Arc<dyn Feature>], hooks.clone()).with_mcp(Arc::new(Mcp)).with_addons(addons, Arc::new(|| json!({}))));
        Fx { app: app(state.clone()), state, hooks, fake }
    }

    struct FakeAddon {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl JobAddon for FakeAddon {
        fn id(&self) -> &str {
            "fake-addon"
        }
        async fn view(&self, job: &Value, _: &Value) -> Option<AddonView> {
            (job["featureId"] == "fake").then(|| AddonView { id: "fake-addon".into(), label: "Fake".into(), state: AddonState::Idle, href: None, error: None, auto_start: false })
        }
        async fn start(&self, _: &Value, _: &Value) -> Result<AddonView, String> {
            self.calls.lock().unwrap().push("start".into());
            Ok(AddonView { id: "fake-addon".into(), label: "Fake".into(), state: AddonState::Starting, href: None, error: None, auto_start: false })
        }
        async fn stop(&self, _: &Value, _: &Value) -> Result<AddonView, String> {
            self.calls.lock().unwrap().push("stop".into());
            Err("nope".into())
        }
        fn heartbeat(&self, origin: &str) -> bool {
            origin == "http://x.localhost"
        }
    }

    #[tokio::test]
    async fn addons_are_listed_started_stopped_and_kept_alive_by_job_and_origin() {
        let addon = Arc::new(FakeAddon { calls: Mutex::new(vec![]) });
        let f = fx_addons(vec![addon.clone()]);
        assert_eq!(get_(&f, "/jobs/nope/addons").await.0, 404);
        let job = f.state.store.create("fake", Map::new());
        let id = job["id"].as_str().unwrap().to_string();
        let (s, body) = get_(&f, &format!("/jobs/{id}/addons")).await;
        assert_eq!((s, body["addons"][0]["state"].clone()), (200, json!("idle")));
        let (s, body) = post_(&f, &format!("/jobs/{id}/addons/fake-addon/start"), "{}").await;
        assert_eq!((s, body["addon"]["state"].clone()), (200, json!("starting")));
        assert_eq!(post_(&f, &format!("/jobs/{id}/addons/fake-addon/stop"), "{}").await.0, 500);
        assert_eq!(post_(&f, &format!("/jobs/{id}/addons/other/start"), "{}").await.0, 404);
        assert_eq!(post_(&f, &format!("/jobs/{id}/addons/fake-addon/launch"), "{}").await.0, 404);
        assert_eq!(post_(&f, "/addons/heartbeat", "{\"origin\":\"http://x.localhost\"}").await.0, 200);
        assert_eq!(post_(&f, "/addons/heartbeat", "{\"origin\":\"http://y.localhost\"}").await.0, 404);
        assert_eq!(post_(&f, "/addons/heartbeat", "{}").await.0, 400);
    }

    async fn call(f: &Fx, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<&str>) -> (u16, HeaderMap, Value) {
        let mut b = axum::http::Request::builder().method(method).uri(uri).header("host", format!("127.0.0.1:{PORT}")).header("x-companion-secret", SECRET);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = f.app.clone().oneshot(b.body(Body::from(body.unwrap_or("").to_string())).unwrap()).await.unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn get_(f: &Fx, uri: &str) -> (u16, Value) {
        let (s, _, v) = call(f, "GET", uri, &[], None).await;
        (s, v)
    }

    async fn post_(f: &Fx, uri: &str, body: &str) -> (u16, Value) {
        let (s, _, v) = call(f, "POST", uri, &[], Some(body)).await;
        (s, v)
    }

    #[tokio::test]
    async fn a_request_not_addressed_to_the_service_is_refused_before_anything_else() {
        let f = fx(false);
        for uri in ["/health", "/features", "/mcp"] {
            let req = axum::http::Request::builder().uri(uri).header("host", "evil.example:8787").header("x-companion-secret", SECRET).body(Body::empty()).unwrap();
            let res = f.app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), 403, "{uri}");
        }
    }

    #[tokio::test]
    async fn health_needs_no_secret_and_sets_no_cors_headers() {
        let f = fx(false);
        let req = axum::http::Request::builder().uri("/health").header("host", "127.0.0.1:8787").header("origin", "https://page.example").body(Body::empty()).unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.headers().get("access-control-allow-origin").is_none());
        let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(v, json!({ "ok": true, "version": "1.2.3", "features": ["fake"] }));
    }

    #[tokio::test]
    async fn every_other_route_needs_the_shared_secret() {
        let f = fx(false);
        for secret in [None, Some("wrong")] {
            let mut b = axum::http::Request::builder().uri("/features").header("host", "127.0.0.1:8787");
            if let Some(s) = secret {
                b = b.header("x-companion-secret", s);
            }
            let res = f.app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(res.status(), 403);
            let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
            assert!(v["error"].as_str().unwrap().contains("aren't using the same secret"));
        }
        assert_eq!(get_(&f, "/nope").await.0, 404);
    }

    #[tokio::test]
    async fn no_response_carries_cors_headers_and_a_preflight_needs_the_secret() {
        let f = fx(false);
        for (method, secret) in [("OPTIONS", None), ("GET", None), ("GET", Some(SECRET))] {
            let mut b = axum::http::Request::builder().method(method).uri("/features").header("host", "127.0.0.1:8787").header("origin", "https://page.example");
            if let Some(s) = secret {
                b = b.header("x-companion-secret", s);
            }
            let res = f.app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
            assert!(res.headers().keys().all(|k| !k.as_str().starts_with("access-control-")), "{method} {secret:?}");
            if secret.is_none() {
                assert_eq!(res.status(), 403);
            }
        }
    }

    #[test]
    fn the_secret_check_wants_the_exact_non_empty_secret() {
        assert!(secret_matches(Some(b"s3cret"), b"s3cret"));
        assert!(!secret_matches(Some(b"s3cre"), b"s3cret"));
        assert!(!secret_matches(Some(b"s3cretx"), b"s3cret"));
        assert!(!secret_matches(Some(b"S3cret"), b"s3cret"));
        assert!(!secret_matches(None, b"s3cret"));
        assert!(!secret_matches(Some(b""), b""), "an unset secret never matches an empty header");
        assert!(!secret_matches(None, b""));
    }

    #[tokio::test]
    async fn mcp_is_passed_whole_to_its_handler_without_the_extension_secret() {
        let f = fx(false);
        let req = axum::http::Request::builder().method("POST").uri("/mcp").header("host", "127.0.0.1:8787").header("authorization", "Bearer t").body(Body::from("{}")).unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);
        let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!((v["method"].clone(), v["auth"].clone(), v["bytes"].clone()), (json!("POST"), json!("Bearer t"), json!(2)));
    }

    #[tokio::test]
    async fn the_feature_list_and_the_caches() {
        let f = fx(false);
        assert_eq!(get_(&f, "/features").await.1, json!([{ "id": "fake", "label": "Fake feature" }]));
        assert_eq!(get_(&f, "/analysis-cache/PROJ-1").await.1, json!({ "exists": true, "completedAt": "2026-01-01T00:00:00.000Z" }));
        assert_eq!(get_(&f, "/analysis-cache/PROJ-2").await.1, json!({ "exists": false, "completedAt": null }));
        assert_eq!(get_(&f, "/summary-cache/PROJ-2").await.1, json!({ "exists": false, "completedAt": null, "commentCount": null }));
    }

    #[tokio::test]
    async fn start_runs_the_checks_tags_the_job_and_answers_the_presented_job() {
        let f = fx(false);
        let (s, v) = call(&f, "POST", "/features/fake/start", &[("x-relay-origin", "https://jira.example"), ("x-relay-cookie", "a=b")], Some("{\"key\":\"k1\"}")).await.2.pipe(|v| (200, v));
        assert_eq!(s, 200);
        assert_eq!((v["presented"].clone(), v["scopeKey"].clone(), v["startedVia"].clone()), (json!(true), json!("k1"), json!("extension")));
        assert_eq!(f.hooks.calls.lock().unwrap()[0], "session:https://jira.example");
        // The same job is then readable by status.
        let (s, status) = get_(&f, &format!("/status/{}", v["id"].as_str().unwrap())).await;
        assert_eq!((s, status["presented"].clone()), (200, json!(true)));
        assert_eq!(get_(&f, "/status/nope").await.0, 404);
        assert_eq!(post_(&f, "/features/nope/start", "{}").await.0, 404);
    }

    trait Pipe: Sized {
        fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
            f(self)
        }
    }
    impl<T> Pipe for T {}

    #[tokio::test]
    async fn status_includes_the_live_session_of_a_feature_that_hands_a_job_off() {
        struct Handoff(Arc<JobStore>);
        #[async_trait]
        impl Feature for Handoff {
            fn id(&self) -> &str {
                "handoff"
            }
            fn label(&self) -> &str {
                "Handoff"
            }
            async fn start(&self, _: Value, _: FeatureContext) -> crate::jobs::FeatureResult<Value> {
                Ok(self.0.create("handoff", Map::new()))
            }
            async fn approve(&self, _: Value, _: FeatureContext) -> crate::jobs::FeatureResult<()> {
                Ok(())
            }
            async fn reject(&self, _: Value, _: FeatureContext) -> crate::jobs::FeatureResult<()> {
                Ok(())
            }
            async fn session_status(&self, _: &Value) -> Option<crate::jobs::SessionStatus> {
                Some(crate::jobs::SessionStatus { state: "running".into(), exit_code: None })
            }
            async fn stop_session(&self, _: Value) -> crate::jobs::FeatureResult<()> {
                Ok(())
            }
        }
        let store = Arc::new(JobStore::new());
        let hooks = Arc::new(Hooks { calls: Mutex::new(vec![]), launchd: false });
        let state = Arc::new(AppState::new(PORT, store.clone(), vec![Arc::new(Handoff(store.clone()))], hooks.clone()));
        let f = Fx { app: app(state.clone()), state, hooks, fake: Arc::new(Fake { store, fail: Mutex::new(None) }) };
        let id = post_(&f, "/features/handoff/start", "{}").await.1["id"].as_str().unwrap().to_string();
        let (_, v) = get_(&f, &format!("/status/{id}")).await;
        assert_eq!(v["session"]["state"], "running");
        // Cancelling an approved job stops the session and answers the job with its status.
        f.state.store.update(&id, Map::from_iter([("status".to_string(), json!("approved"))])).unwrap();
        let (s, v) = post_(&f, &format!("/features/handoff/{id}/cancel"), "").await;
        assert_eq!((s, v["session"]["state"].clone()), (200, json!("running")));
    }

    #[tokio::test]
    async fn a_start_refused_for_a_missing_clone_or_by_the_feature_is_a_409_with_its_details() {
        let f = fx(false);
        let (s, v) = post_(&f, "/features/fake/start", "{\"project\":\"MISSING\"}").await;
        assert_eq!((s, v["code"].clone(), v["project"].clone()), (409, json!("repo-not-found"), json!("MISSING")));
        *f.fake.fail.lock().unwrap() = Some(FeatureError::refused("busy", "review-in-progress", Map::from_iter([("existing".to_string(), json!({ "jobId": "j" }))])));
        let (s, v) = post_(&f, "/features/fake/start", "{}").await;
        assert_eq!((s, v["error"].clone(), v["code"].clone(), v["existing"]["jobId"].clone()), (409, json!("busy"), json!("review-in-progress"), json!("j")));
        assert_eq!(post_(&f, "/features/fake/start", "{not json").await.0, 400);
    }

    #[tokio::test]
    async fn errors_from_a_feature_map_to_409_410_and_500() {
        let f = fx(false);
        let id = post_(&f, "/features/fake/start", "{}").await.1["id"].as_str().unwrap().to_string();
        for (err, status) in [(FeatureError::JobStatus("x".into()), 409), (FeatureError::WorktreeGone("g".into()), 410), (FeatureError::Other("o".into()), 500)] {
            *f.fake.fail.lock().unwrap() = Some(err);
            assert_eq!(post_(&f, &format!("/features/fake/{id}/approve"), "{}").await.0, status);
        }
        assert_eq!(post_(&f, "/features/fake/nope/approve", "{}").await.0, 404);
        let (s, v) = post_(&f, &format!("/features/fake/{id}/approve"), "{}").await;
        assert_eq!((s, v["status"].clone(), v["presented"].clone()), (200, json!("approved"), json!(true)));
    }

    #[tokio::test]
    async fn cancel_rejects_a_finished_job_and_answers_the_job() {
        let f = fx(false);
        let id = post_(&f, "/features/fake/start", "{}").await.1["id"].as_str().unwrap().to_string();
        // Still "running": the feature's own cancel.
        let (s, v) = post_(&f, &format!("/features/fake/{id}/cancel"), "").await;
        assert_eq!((s, v["status"].clone()), (200, json!("rejected")));
        // A job awaiting approval is rejected.
        let id2 = post_(&f, "/features/fake/start", "{}").await.1["id"].as_str().unwrap().to_string();
        f.state.store.update(&id2, Map::from_iter([("status".to_string(), json!("awaiting-approval"))])).unwrap();
        assert_eq!(post_(&f, &format!("/features/fake/{id2}/cancel"), "").await.1["status"], "rejected");
    }

    #[tokio::test]
    async fn an_action_runs_on_its_own_jobs_and_an_unknown_one_is_a_404() {
        let f = fx(false);
        let id = post_(&f, "/features/fake/start", "{}").await.1["id"].as_str().unwrap().to_string();
        let (s, v) = post_(&f, &format!("/features/fake/{id}/actions/ping"), "{\"a\":1}").await;
        assert_eq!((s, v["pong"].clone()), (200, json!({ "a": 1 })));
        assert_eq!(post_(&f, &format!("/features/fake/{id}/actions/nope"), "{}").await.0, 404);
        assert_eq!(post_(&f, "/features/fake/zzz/actions/ping", "{}").await.0, 404);
        // This feature has no post-comment or refresh-diff.
        assert_eq!(post_(&f, &format!("/features/fake/{id}/post-comment"), "{}").await.0, 404);
        assert_eq!(post_(&f, &format!("/features/fake/{id}/refresh-diff"), "{}").await.0, 404);
    }

    #[tokio::test]
    async fn lookup_needs_a_valid_scope_key_and_a_running_feature_then_finds_the_tagged_job() {
        let f = fx(false);
        assert_eq!(get_(&f, "/jobs/lookup").await.0, 400);
        assert_eq!(get_(&f, "/jobs/lookup?featureId=nope&scopeKey=pr:a/b%231").await.0, 400);
        assert_eq!(get_(&f, "/jobs/lookup?featureId=fake&scopeKey=%00bad").await.0, 400);
        let (s, v) = get_(&f, "/jobs/lookup?featureId=fake&scopeKey=jira:PROJ-1").await;
        assert_eq!((s, v), (200, json!({ "job": null })));
        // An extension-started job is never looked up this way: the page already has its id.
        post_(&f, "/features/fake/start", "{\"key\":\"jira:PROJ-1\"}").await;
        assert_eq!(get_(&f, "/jobs/lookup?featureId=fake&scopeKey=jira:PROJ-1").await.1, json!({ "job": null }));
        let pid = f.state.store.create_pending("fake", json!({ "key": "jira:PROJ-1" }), None)["id"].as_str().unwrap().to_string();
        post_(&f, &format!("/jobs/{pid}/start"), "{}").await;
        let (_, v) = get_(&f, "/jobs/lookup?featureId=fake&scopeKey=jira:PROJ-1").await;
        assert_eq!((v["job"]["presented"].clone(), v["job"]["startedVia"].clone()), (json!(true), json!("mcp")));
    }

    #[tokio::test]
    async fn a_pending_start_job_is_started_once_and_superseded_or_dismissed() {
        let f = fx(false);
        let pending = f.state.store.create_pending("fake", json!({ "x": 1 }), None);
        let pid = pending["id"].as_str().unwrap().to_string();
        let (s, v) = post_(&f, &format!("/jobs/{pid}/start"), "{}").await;
        assert_eq!((s, v["startedVia"].clone()), (200, json!("mcp")));
        let old = f.state.store.get(&pid).unwrap();
        assert_eq!((old["status"].clone(), old["data"]["supersededBy"].clone()), (json!("rejected"), v["id"].clone()));
        // No longer pending.
        assert_eq!(post_(&f, &format!("/jobs/{pid}/start"), "{}").await.0, 409);
        assert_eq!(post_(&f, &format!("/jobs/{pid}/dismiss"), "{}").await.0, 409);
        let p2 = f.state.store.create_pending("fake", json!({}), None)["id"].as_str().unwrap().to_string();
        assert_eq!(post_(&f, &format!("/jobs/{p2}/dismiss"), "{}").await, (200, json!({ "ok": true })));
        assert_eq!(f.state.store.get(&p2).unwrap()["status"], "rejected");
        assert_eq!(post_(&f, "/jobs/nope/start", "{}").await.0, 404);
        // A pending job whose feature isn't enabled.
        let p3 = f.state.store.create_pending("other", json!({}), None)["id"].as_str().unwrap().to_string();
        assert_eq!(post_(&f, &format!("/jobs/{p3}/start"), "{}").await.0, 400);
    }

    #[tokio::test]
    async fn a_failed_start_leaves_the_pending_job_as_it_was() {
        let f = fx(false);
        let pid = f.state.store.create_pending("fake", json!({ "project": "MISSING" }), None)["id"].as_str().unwrap().to_string();
        assert_eq!(post_(&f, &format!("/jobs/{pid}/start"), "{}").await.0, 409);
        assert_eq!(f.state.store.get(&pid).unwrap()["status"], "pending-start");
    }

    #[tokio::test]
    async fn history_metrics_validates_days_and_the_extension_hello_is_remembered() {
        let f = fx(false);
        assert_eq!(get_(&f, "/history/metrics").await, (200, json!({ "days": 30 })));
        assert_eq!(get_(&f, "/history/metrics?days=7").await.1, json!({ "days": 7 }));
        for bad in ["0", "3651", "1.5", "x"] {
            assert_eq!(get_(&f, &format!("/history/metrics?days={bad}")).await.0, 400, "{bad}");
        }
        assert_eq!(get_(&f, "/extension/hello").await.1, json!({ "connected": false }));
        assert_eq!(post_(&f, "/extension/hello", "{\"version\":\"0.9\"}").await, (200, json!({ "ok": true, "heartbeat": true })));
        let (_, v) = get_(&f, "/extension/hello").await;
        assert_eq!((v["connected"].clone(), v["version"].clone()), (json!(true), json!("0.9")));
        assert_eq!(get_(&f, "/extension/session-hosts").await.1["hosts"], json!(["https://jira.example"]));
        assert_eq!(get_(&f, "/extension/session-origins").await.1, json!({ "hosts": [] }));
    }

    #[tokio::test]
    async fn unavailable_hooks_answer_501_and_settings_come_from_the_hook() {
        let f = fx(false);
        assert_eq!(get_(&f, "/settings").await, (200, json!({ "settings": {} })));
        assert_eq!(get_(&f, "/data-folder").await.0, 501);
        assert_eq!(post_(&f, "/open-location", "{}").await.0, 501);
        assert_eq!(get_(&f, "/update").await.0, 500);
        assert_eq!(post_(&f, "/update/apply", "").await.0, 400);
    }

    #[tokio::test]
    async fn the_data_folder_purge_waits_for_running_jobs() {
        let f = fx(false);
        post_(&f, "/features/fake/start", "{}").await;
        // The fake's job has no status yet: set it running.
        let id = f.state.store.create("fake", Map::new())["id"].as_str().unwrap().to_string();
        f.state.store.update(&id, Map::from_iter([("status".to_string(), json!("running"))])).unwrap();
        let (s, v) = post_(&f, "/data-folder/purge", "").await;
        assert_eq!((s, v["code"].clone()), (409, json!("jobs-running")));
    }

    #[tokio::test]
    async fn restart_needs_launchd_and_no_running_jobs_unless_forced() {
        let f = fx(false);
        let (s, v) = post_(&f, "/settings/restart", "{}").await;
        assert_eq!(s, 400);
        assert!(v["error"].as_str().unwrap().contains("npm start"));

        let f = fx(true);
        let id = f.state.store.create("fake", Map::new())["id"].as_str().unwrap().to_string();
        f.state.store.update(&id, Map::from_iter([("status".to_string(), json!("running"))])).unwrap();
        let (s, v) = post_(&f, "/settings/restart", "{}").await;
        assert_eq!((s, v["activeJobs"].clone()), (409, json!(1)));
        assert!(f.hooks.calls.lock().unwrap().is_empty());
        assert_eq!(post_(&f, "/settings/restart", "{\"force\":true}").await.0, 200);
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(*f.hooks.calls.lock().unwrap(), ["restart"]);
    }

    #[tokio::test]
    async fn open_in_claude_code_uses_only_the_jobs_own_worktree() {
        let f = fx(false);
        assert_eq!(post_(&f, "/jobs/nope/open-in-claude-code", "{}").await.0, 404);
        let job = f.state.store.create("fake", Map::new());
        let id = job["id"].as_str().unwrap().to_string();
        assert_eq!(post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{}").await.0, 400);
        let dir = tempfile::tempdir().unwrap();
        f.state.store.patch_data(&id, Map::from_iter([("worktree".to_string(), json!({ "dir": dir.path() }))])).unwrap();
        assert_eq!(post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{}").await, (200, json!({ "ok": true })));
        let label = f.state.feature("fake").unwrap().label().to_string();
        assert_eq!(f.hooks.calls.lock().unwrap().last().unwrap(), &format!("open:{}:{label}", dir.path().display()));
        // Where it opened is kept on the job, titled by feature (and the PR or ticket, when it has one).
        assert_eq!(f.state.store.get(&id).unwrap()["data"]["terminal"]["windowId"], "7");
        f.state.store.patch_data(&id, Map::from_iter([("worktree".to_string(), json!({ "dir": "/definitely/not/here" }))])).unwrap();
        assert_eq!(post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{}").await.0, 410);
    }

    #[tokio::test]
    async fn focus_terminal_uses_only_the_location_the_service_recorded() {
        let f = fx(false);
        assert_eq!(post_(&f, "/jobs/nope/focus-terminal", "{}").await.0, 404);
        let id = f.state.store.create("fake", Map::new())["id"].as_str().unwrap().to_string();
        // Nothing recorded: nothing is focused and the hook isn't called.
        assert_eq!(post_(&f, &format!("/jobs/{id}/focus-terminal"), "{}").await, (200, json!({ "focus": "unknown" })));
        assert!(f.hooks.calls.lock().unwrap().is_empty());
        f.state.store.set_data(&id, "terminal", json!({ "app": "terminal", "windowId": "7", "tty": "/dev/ttys003" })).unwrap();
        assert_eq!(post_(&f, &format!("/jobs/{id}/focus-terminal"), "{\"location\":{\"tty\":\"/dev/ttys999\"}}").await, (200, json!({ "focus": "focused" })));
        assert_eq!(f.hooks.calls.lock().unwrap().last().unwrap(), "focus:/dev/ttys003");
        // A window that has been closed.
        f.state.store.set_data(&id, "terminal", json!({ "app": "terminal", "windowId": "9", "tty": "/dev/ttys003" })).unwrap();
        assert_eq!(post_(&f, &format!("/jobs/{id}/focus-terminal"), "{}").await, (200, json!({ "focus": "missing" })));
    }

    #[tokio::test]
    async fn resume_is_refused_while_running_and_answers_per_outcome() {
        let f = fx(false);
        let id = f.state.store.create("fake", Map::new())["id"].as_str().unwrap().to_string();
        f.state.store.update(&id, Map::from_iter([("status".to_string(), json!("running"))])).unwrap();
        assert_eq!(post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{\"resume\":true}").await.0, 409);
        f.state.store.update(&id, Map::from_iter([("status".to_string(), json!("awaiting-approval"))])).unwrap();
        // No recorded session: invalid.
        assert_eq!(post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{\"resume\":true}").await.0, 400);
        f.state.store.patch_data(&id, Map::from_iter([("claudeSession".to_string(), json!({ "id": "abc" })), ("issueKey".to_string(), json!("PROJ-1"))])).unwrap();
        assert_eq!(post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{\"resume\":true}").await.0, 200);
        // The header: rule, title, rule, status, blank.
        assert_eq!(f.hooks.calls.lock().unwrap().last().unwrap(), "resume:\"abc\":5");
        f.state.store.patch_data(&id, Map::from_iter([("claudeSession".to_string(), json!({ "id": "gone" }))])).unwrap();
        let (s, v) = post_(&f, &format!("/jobs/{id}/open-in-claude-code"), "{\"resume\":true}").await;
        assert_eq!(s, 410);
        assert!(v["error"].as_str().unwrap().starts_with("This Claude Code session has expired"));
    }

    #[test]
    fn the_resume_header_names_the_feature_and_the_pr_or_issue() {
        let line = |h: &[Vec<HeaderSegment>], i: usize| h[i].iter().map(|s| s.text.as_str()).collect::<String>();
        let pr = build_resume_header("Resolve", &json!({ "data": { "payload": { "prId": 42 } } }));
        assert_eq!(line(&pr, 1), "Resolve  PR #42");
        let issue = build_resume_header("Analyze", &json!({ "data": { "issueKey": "PROJ-1" } }));
        assert_eq!(line(&issue, 1), "Analyze  PROJ-1");
        assert_eq!(line(&build_resume_header("X", &json!({})), 1), "X");
        assert!(expired_session_message("analyze-issue").contains("Re-analyze"));
        assert!(expired_session_message("resolve-conflict").contains("Open in Claude Code"));
        assert!(expired_session_message("other").ends_with("gone)."));
    }
}
