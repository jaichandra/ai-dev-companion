//! The companion's MCP server, served statelessly at `POST /mcp` (port of `core/mcp.ts`). Its own
//! trust boundary, separate from every other route:
//!
//!   - A bearer token of its own (`mcp.token` in the credential store, rotatable from Settings),
//!     accepted ONLY here; the extension's `X-Companion-Secret` is never accepted here, not even
//!     compared. So a leaked `~/.claude.json` (where Claude Code keeps this token) can call these
//!     tools but can't drive any of the extension's routes.
//!   - A Host check (`mcp_auth::host_allowed`) against DNS rebinding.
//!   - No MCP tool pushes, posts, comments or writes config: they read, run the read-only
//!     analyze-issue / pre-deployment-stats jobs, or only *queue* a write-capable job as
//!     "pending-start", which runs only once the user clicks Start on the PR's page in Chrome; the one
//!     write, `forget_item`, only deletes the companion's own local history, and `open_location` only
//!     opens a tracked file in a local editor.
//!
//! Framework-agnostic: the HTTP layer (axum) mounts it with two calls and no more.
//!
//! ```text
//! // Before the body is read or parsed (so nothing unauthenticated is ever parsed: a 5 MB body
//! // without the token is a 401, not a 413). POST only; anything else is `not_allowed()`.
//! pub fn gate(req: &McpRequest<'_>, deps: &McpDeps) -> Option<McpResponse>
//! // The whole request, once the body is in (repeats the access check):
//! pub async fn handle_mcp(req: &McpRequest<'_>, body: &[u8], deps: &McpDeps) -> McpResponse
//! pub fn not_allowed() -> McpResponse                 // 405 + `Allow: POST` for GET/DELETE
//! pub fn body_too_large() -> McpResponse              // 413, JSON-RPC shaped (parser's size limit)
//! pub struct McpRequest<'a> { pub method: &'a str, pub host: Option<&'a str>, pub authorization: Option<&'a str> }
//! pub struct McpResponse { pub status: u16, pub headers: Vec<(String, String)>, pub body: Option<Value> }
//! ```
//!
//! A `None` body is an empty answer (202 for a notification). Stateless: a fresh handler per
//! request, no sessions, no SSE stream.
//!
//! Tools beyond the built-in ones (a feature's own `mcp_tools`, `find_similar`, `get_change_risk`,
//! whose clients are generic) are handed in as `extra_handlers`, keyed by tool name; the catalog says
//! which tools exist and which feature each belongs to.

use crate::atlassian::AuthContext;
use crate::config::ConfigFn;
use crate::feature_registry::FeatureRegistry;
use crate::features::address_review_comments::plan::collect_open_comments;
use crate::guard::mcp_classifier::COMPANION_MCP_SERVER;
use crate::history_db::HistoryDb;
use crate::js_text::{slice_utf16, utf16_len};
use crate::jobs::JobStore;
use crate::background_routes::SharedInbox;
use crate::mcp_auth::{bearer_matches, host_allowed};
use crate::mcp_background_tools::list_notifications;
use crate::mcp_tools::{cap_tool_output, input_schema_for, select_tools, validate_tool_args, ToolDef, DEFAULT_MAX_OUTPUT_CHARS};
use crate::open_location::OpenLocation;
use crate::prereqs::GitRemoteRules;
use crate::providers::Providers;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const LIST_JOBS_LIMIT: usize = 50;
pub const DIFF_MAX_CHARS: usize = 20_000;
const AWAIT_POLL: Duration = Duration::from_millis(500);
const JIRA_ISSUE_FIELDS: &str = "summary,status,issuetype,priority,assignee,reporter,components,labels,fixVersions,versions,description,environment,comment";
const JIRA_DESCRIPTION_MAX_CHARS: usize = 20_000;
const JIRA_COMMENT_LIMIT: usize = 20;
const JIRA_COMMENT_BODY_MAX_CHARS: usize = 4_000;
const BITBUCKET_COMMENT_TEXT_MAX_CHARS: usize = 4_000;
const SUPPORTED_PROTOCOLS: [&str; 4] = ["2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What a tool handler may do besides reading its arguments.
#[derive(Clone)]
pub struct McpToolContext {
    /// Empty on purpose: every authed call resolves auth through the credential vault, then the
    /// saved token. An MCP call has no browser cookie of its own.
    pub auth: AuthContext,
    /// Starts an enabled feature's job now (analyze-issue); the job as stored.
    pub start_job: StartJobFn,
    /// Creates a "pending-start" job for the extension's Start click.
    pub create_pending_job: PendingJobFn,
    /// Resolves with the job once it leaves "running" or the timeout passes.
    pub await_job: AwaitJobFn,
}

pub type StartJobFn = Arc<dyn Fn(String, Value) -> BoxFut<Result<Value, String>> + Send + Sync>;
pub type PendingJobFn = Arc<dyn Fn(&str, &Value) -> Result<Value, String> + Send + Sync>;
pub type AwaitJobFn = Arc<dyn Fn(String, Duration) -> BoxFut<Result<Value, String>> + Send + Sync>;

pub type McpHandler = Arc<dyn Fn(Value, McpToolContext) -> BoxFut<Result<Value, String>> + Send + Sync>;

/// How to make the key a "pending-start" job is stored under (`scope_key::scope_key_for`).
pub type ScopeKeyFn = Arc<dyn Fn(&str, &Value) -> Option<String> + Send + Sync>;

pub struct McpDeps {
    pub config: ConfigFn,
    pub home: PathBuf,
    pub version: String,
    pub port: u16,
    pub git: GitRemoteRules,
    /// The saved MCP token; an `Err` is a credential-store read error.
    pub get_token: Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>,
    pub store: Arc<JobStore>,
    pub providers: Providers,
    pub catalog: Vec<ToolDef>,
    pub features: Arc<FeatureRegistry>,
    pub enabled_feature_ids: Vec<String>,
    /// `None` when local history is off.
    pub history: Option<Arc<HistoryDb>>,
    /// Starts a job through the server's one start path (`via` "mcp"); the job as stored.
    pub start_feature_job: StartJobFn,
    /// Names of the user's own configured MCP servers.
    pub read_mcp_servers: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    pub notifications: Option<SharedInbox>,
    pub open_location: Option<Arc<OpenLocation>>,
    pub scope_key_for: ScopeKeyFn,
    pub extra_handlers: HashMap<String, McpHandler>,
}

pub struct McpRequest<'a> {
    pub method: &'a str,
    pub host: Option<&'a str>,
    pub authorization: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl McpResponse {
    fn json(status: u16, body: Value) -> Self {
        Self { status, headers: vec![], body: Some(body) }
    }
}

fn rpc_error(code: i64, message: &str, id: Value) -> Value {
    json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message }, "id": id })
}

fn log_failure(at: &str, err: &str) {
    // Only ever the error's message: never a request's arguments, and nothing here holds the token.
    eprintln!("[mcp] {at} failed: {err}");
}

/// The /mcp access check, shared by `gate` and `handle_mcp` (a second check, in case the gate is
/// ever mounted wrong): the Host check, then the token's existence, then the bearer.
/// `X-Companion-Secret` is deliberately never looked at. `Err` is a credential-store read error.
fn access_refusal(req: &McpRequest<'_>, deps: &McpDeps) -> Result<Option<McpResponse>, String> {
    if !host_allowed(req.host, deps.port) {
        let port = deps.port;
        return Ok(Some(McpResponse::json(403, json!({ "error": format!("/mcp only answers requests addressed to 127.0.0.1:{port} or localhost:{port}.") }))));
    }
    let Some(token) = (deps.get_token)()?.filter(|t| !t.is_empty()) else {
        return Ok(Some(McpResponse::json(503, json!({ "error": "The companion's MCP token isn't set up yet \u{2014} run the installer again (`install --yes`) or `companion setup`." }))));
    };
    if !bearer_matches(req.authorization, Some(&token)) {
        let mut r = McpResponse::json(401, json!({ "error": "Missing or wrong MCP token." }));
        r.headers.push(("WWW-Authenticate".into(), "Bearer".into()));
        return Ok(Some(r));
    }
    Ok(None)
}

fn refusal_or_500(req: &McpRequest<'_>, deps: &McpDeps) -> Option<McpResponse> {
    match access_refusal(req, deps) {
        Ok(r) => r,
        Err(e) => {
            log_failure("access check", &e);
            Some(McpResponse::json(500, json!({ "error": "The companion couldn't check the MCP token." })))
        }
    }
}

/// Mounted on /mcp BEFORE the body is read. Only POST is gated: GET/DELETE get `not_allowed`.
pub fn gate(req: &McpRequest<'_>, deps: &McpDeps) -> Option<McpResponse> {
    if req.method != "POST" {
        return None;
    }
    refusal_or_500(req, deps)
}

/// Stateless: no SSE stream to GET, no session to DELETE.
pub fn not_allowed() -> McpResponse {
    let mut r = McpResponse::json(405, json!({ "error": "Only POST is supported on /mcp." }));
    r.headers.push(("Allow".into(), "POST".into()));
    r
}

pub fn body_too_large() -> McpResponse {
    McpResponse::json(413, rpc_error(-32600, "Request too large", Value::Null))
}

// ---- built-in tools -------------------------------------------------------------------------

fn text_of(v: &Value) -> Option<&str> {
    v.as_str()
}

fn list_jobs(store: &JobStore, args: &Value) -> Value {
    let jobs: Vec<Value> = store
        .list()
        .into_iter()
        .filter(|j| args.get("featureId").is_none_or(|f| j["featureId"] == *f))
        .filter(|j| args.get("scopeKey").is_none_or(|s| j["scopeKey"] == *s))
        .take(LIST_JOBS_LIMIT)
        .map(|j| {
            let mut m = Map::new();
            m.insert("id".into(), j["id"].clone());
            m.insert("featureId".into(), j["featureId"].clone());
            m.insert("status".into(), j["status"].clone());
            // Left out when absent, as `undefined` is by JSON.stringify.
            for (to, v) in [("scopeKey", &j["scopeKey"]), ("startedVia", &j["startedVia"]), ("createdAt", &j["createdAt"]), ("updatedAt", &j["updatedAt"]), ("summary", &j["result"]["summary"]), ("error", &j["error"]), ("progress", &j["progress"]["label"])] {
                if !v.is_null() {
                    m.insert(to.into(), v.clone());
                }
            }
            Value::Object(m)
        })
        .collect();
    Value::Array(jobs)
}

fn get_job(store: &JobStore, args: &Value) -> Result<Value, String> {
    let job = store.get(args["jobId"].as_str().unwrap_or("")).ok_or("No job with that id.")?;
    if job["result"].is_null() {
        return Ok(job);
    }
    // A copy: never trim the stored job itself, which the extension still renders in full.
    let mut out = job.clone();
    if let Some(files) = out["result"]["files"].as_array_mut() {
        for file in files {
            let diff = file["diff"].as_str().unwrap_or("").to_string();
            let len = utf16_len(&diff);
            if len > DIFF_MAX_CHARS {
                file["diff"] = json!(format!("{}\n\u{2026} (truncated: {} more characters \u{2014} open the job in Chrome for the full diff)", slice_utf16(&diff, DIFF_MAX_CHARS), len - DIFF_MAX_CHARS));
            }
        }
    }
    Ok(out)
}

/// `lookup_local_repo`: a read-only lookup, NOT `Config::repo_path` (which writes config.json: it
/// saves an inferred mapping, and drops a stale one). No MCP tool writes config.
fn lookup_local_repo(config: &Value, home: &Path, args: &Value) -> Value {
    let (project, repo) = (args["project"].as_str().unwrap_or(""), args["repo"].as_str().unwrap_or(""));
    let key = format!("{project}/{repo}").to_lowercase();
    let repos = config["repos"].as_object().cloned().unwrap_or_default();
    if let Some((_, p)) = repos.iter().find(|(k, _)| k.to_lowercase() == key) {
        if let Some(p) = p.as_str().filter(|p| Path::new(p).join(".git").exists()) {
            return json!({ "path": p, "source": "configured" });
        }
    }
    let existing: Vec<PathBuf> = repos.values().filter_map(Value::as_str).map(PathBuf::from).collect();
    match crate::prereqs::infer_repo_path(&existing, project, repo, &crate::prereqs::default_search_roots(home)) {
        Some(p) => json!({ "path": p.to_string_lossy(), "source": "inferred", "note": "Not saved to config \u{2014} the first job for this repo will save it." }),
        None => json!({ "path": null, "source": "none" }),
    }
}

/// A user reference's display name, falling back to the username.
fn jira_user_name(user: &Value) -> Value {
    for k in ["displayName", "name"] {
        if let Some(s) = user[k].as_str().filter(|s| !s.is_empty()) {
            return json!(s);
        }
    }
    Value::Null
}

fn names(v: &Value) -> Vec<Value> {
    v.as_array().into_iter().flatten().filter_map(|x| x["name"].as_str().filter(|n| !n.is_empty())).map(|n| json!(n)).collect()
}

/// `jira_get_issue`: a compact read of one issue. This Jira Server instance's description and
/// comment bodies are wiki-markup strings, so they are capped by length rather than parsed.
async fn jira_get_issue(deps: &McpDeps, args: &Value, ctx: &McpToolContext) -> Result<Value, String> {
    let key = args["issueKey"].as_str().unwrap_or("");
    let issue = deps.providers.issues.get_issue_raw(&ctx.auth, key, JIRA_ISSUE_FIELDS).await.map_err(|e| e.to_string())?;
    let fields = &issue["fields"];
    let all: Vec<Value> = fields["comment"]["comments"].as_array().cloned().unwrap_or_default();
    let comments: Vec<Value> = all[all.len().saturating_sub(JIRA_COMMENT_LIMIT)..]
        .iter()
        .map(|c| json!({ "author": jira_user_name(&c["author"]), "created": c["created"].as_str(), "body": c["body"].as_str().map_or(String::new(), |b| slice_utf16(b, JIRA_COMMENT_BODY_MAX_CHARS).to_string()) }))
        .collect();
    Ok(json!({
        "key": issue["key"].as_str().filter(|k| !k.is_empty()).unwrap_or(key),
        "summary": fields["summary"].as_str(),
        "status": fields["status"]["name"].as_str(),
        "type": fields["issuetype"]["name"].as_str(),
        "priority": fields["priority"]["name"].as_str(),
        "assignee": jira_user_name(&fields["assignee"]),
        "reporter": jira_user_name(&fields["reporter"]),
        "components": names(&fields["components"]),
        "labels": fields["labels"].as_array().cloned().unwrap_or_default(),
        "fixVersions": names(&fields["fixVersions"]),
        "description": fields["description"].as_str().map(|d| slice_utf16(d, JIRA_DESCRIPTION_MAX_CHARS).to_string()),
        "comments": comments,
    }))
}

fn pr_args(args: &Value) -> (String, String, i64) {
    (args["project"].as_str().unwrap_or("").to_string(), args["repo"].as_str().unwrap_or("").to_string(), args["prId"].as_i64().or_else(|| args["prId"].as_f64().map(|f| f as i64)).unwrap_or(0))
}

async fn bitbucket_get_pull_request(deps: &McpDeps, args: &Value, ctx: &McpToolContext) -> Result<Value, String> {
    let (project, repo, id) = pr_args(args);
    Ok(deps.providers.git.get_pull_request(&ctx.auth, &project, &repo, id).await.map_err(|e| e.to_string())?.unwrap_or(Value::Null))
}

/// `bitbucket_list_open_comments`: address-review-comments' own `collect_open_comments` (the same
/// "still open" rules the job uses), reduced to the fields worth handing to a model. `me` is
/// best-effort: a whoAmI failure degrades to "unknown user" rather than failing the whole read.
async fn bitbucket_list_open_comments(deps: &McpDeps, args: &Value, ctx: &McpToolContext) -> Result<Value, String> {
    let (project, repo, id) = pr_args(args);
    let git = &deps.providers.git;
    let me = git.who_am_i(&ctx.auth).await.ok().flatten();
    let activities = git.list_activities(&ctx.auth, &project, &repo, id).await.map_err(|e| e.to_string())?;
    let open = collect_open_comments(&Value::Array(activities), me.as_deref(), None);
    Ok(Value::Array(
        open.iter()
            .map(|c| {
                json!({
                    "id": c["id"],
                    "author": c["authorSlug"].as_str(),
                    "severity": c["severity"].as_str(),
                    "state": c["state"].as_str(),
                    "path": c["anchor"]["path"].as_str(),
                    "line": c["anchor"]["line"].as_f64().map(|l| json!(l)).unwrap_or(Value::Null),
                    "text": c["text"].as_str().map_or(String::new(), |t| slice_utf16(t, BITBUCKET_COMMENT_TEXT_MAX_CHARS).to_string()),
                })
            })
            .collect(),
    ))
}

/// The PR check both queueing tools share (start_resolve_conflict, start_address_review_comments):
/// the PR must exist, be open and have both branches, checked now so Claude hears about a bad PR
/// number straight away instead of the user finding a pending job that can't start.
pub async fn fetch_open_pull_request(providers: &Providers, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<(String, String), String> {
    let pr = providers.git.get_pull_request(auth, project, repo, pr_id).await.map_err(|e| e.to_string())?;
    let Some(pr) = pr.filter(|p| !p.is_null()) else { return Err(format!("Bitbucket didn't return pull request {project}/{repo} #{pr_id}.")) };
    if pr["state"] != "OPEN" {
        let state = pr["state"].as_str().filter(|s| !s.is_empty()).map_or("not open".to_string(), str::to_lowercase);
        return Err(format!("Pull request {project}/{repo} #{pr_id} is {state}, not open."));
    }
    match (pr["fromBranch"].as_str().filter(|b| !b.is_empty()), pr["toBranch"].as_str().filter(|b| !b.is_empty())) {
        (Some(f), Some(t)) => Ok((f.into(), t.into())),
        _ => Err(format!("Bitbucket didn't report the source and target branches of {project}/{repo} #{pr_id}.")),
    }
}

/// What a queueing tool returns once its "pending-start" job exists.
pub fn pending_start_result(providers: &Providers, job: &Value, project: &str, repo: &str, pr_id: i64) -> Result<Value, String> {
    Ok(json!({
        "jobId": job["id"],
        "status": "pending-start",
        "prUrl": providers.git.pr_url(project, repo, pr_id).map_err(|e| e.to_string())?,
        "message": "Queued. Open the PR in Chrome and click Start in the \u{2728} panel \u{2014} nothing runs until then.",
    }))
}

/// Resolves with the job once it leaves "running" or `timeout` passes, and, should it vanish from
/// the store mid-poll, with the last snapshot seen. Only a job that never existed rejects.
pub async fn await_job(store: Arc<JobStore>, job_id: String, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let mut last = store.get(&job_id).ok_or("No job with that id.")?;
    loop {
        let Some(job) = store.get(&job_id) else { return Ok(last) };
        last = job.clone();
        if job["status"] != "running" || Instant::now() >= deadline {
            return Ok(job);
        }
        tokio::time::sleep(AWAIT_POLL).await;
    }
}

fn require_history(deps: &McpDeps) -> Result<&Arc<HistoryDb>, String> {
    deps.history.as_ref().ok_or_else(|| "Local history is off on this copy of the companion (it only records on the installed copy).".to_string())
}

/// The tools this module serves itself; `find_similar` and `get_change_risk` come in as extra handlers.
const BUILT_IN: [&str; 11] = [
    "list_jobs", "get_job", "lookup_local_repo", "get_history", "list_history", "forget_item", "open_location", "list_notifications", "jira_get_issue", "bitbucket_get_pull_request",
    "bitbucket_list_open_comments",
];

fn has_handler(deps: &McpDeps, name: &str) -> bool {
    deps.extra_handlers.contains_key(name) || BUILT_IN.contains(&name)
}

async fn run_handler(deps: &McpDeps, name: &str, args: Value, ctx: McpToolContext) -> Result<Value, String> {
    if let Some(h) = deps.extra_handlers.get(name) {
        return h(args, ctx).await;
    }
    let config = (deps.config)();
    match name {
        "list_jobs" => Ok(list_jobs(&deps.store, &args)),
        "get_job" => get_job(&deps.store, &args),
        "lookup_local_repo" => Ok(lookup_local_repo(&config, &deps.home, &args)),
        "get_history" => require_history(deps)?.get_item(args["key"].as_str().unwrap_or(""))?.ok_or_else(|| "Nothing is recorded under that key.".to_string()),
        "list_history" => {
            let history = require_history(deps)?;
            let days = args["days"].as_i64().unwrap_or(30);
            let matches = match args["query"].as_str() {
                Some(q) => history.search(q, Some(20))?,
                None => vec![],
            };
            Ok(json!({ "days": days, "metrics": history.metrics(days, None)?, "prewarmed": history.prewarm_metrics(days, None)?, "matches": matches }))
        }
        "forget_item" => {
            let (items, events) = require_history(deps)?.forget(args["key"].as_str().unwrap_or(""))?;
            Ok(json!({ "items": items, "events": events }))
        }
        "open_location" => {
            let Some(open) = &deps.open_location else { return Err("Open in editor isn't available on this copy of the companion.".into()) };
            match open.open_location(&config, text_of(&args["text"]), None).await {
                Ok(o) => Ok(serde_json::to_value(o).map_err(|e| e.to_string())?),
                Err(e) => Err(e.message),
            }
        }
        "list_notifications" => {
            let mut guard = deps.notifications.as_ref().map(|n| n.lock().unwrap_or_else(|p| p.into_inner()));
            list_notifications(guard.as_deref_mut(), &args)
        }
        "jira_get_issue" => jira_get_issue(deps, &args, &ctx).await,
        "bitbucket_get_pull_request" => bitbucket_get_pull_request(deps, &args, &ctx).await,
        "bitbucket_list_open_comments" => bitbucket_list_open_comments(deps, &args, &ctx).await,
        other => Err(format!("Unknown tool {other}")),
    }
}

fn tool_context(deps: &McpDeps, features_enabled: Vec<String>) -> McpToolContext {
    let start = deps.start_feature_job.clone();
    let (store, scope_key_for) = (deps.store.clone(), deps.scope_key_for.clone());
    let pending_store = deps.store.clone();
    McpToolContext {
        auth: AuthContext::default(),
        start_job: Arc::new(move |feature_id, payload| {
            if !features_enabled.contains(&feature_id) {
                return Box::pin(async move { Err(format!("Feature \"{feature_id}\" isn't enabled.")) });
            }
            start(feature_id, payload)
        }),
        create_pending_job: Arc::new(move |feature_id, payload| {
            let key = scope_key_for(feature_id, payload).ok_or_else(|| format!("Can't queue a {feature_id} job for that payload."))?;
            Ok(pending_store.create_pending(feature_id, payload.clone(), Some(&key)))
        }),
        await_job: Arc::new(move |id, timeout| Box::pin(await_job(store.clone(), id, timeout))),
    }
}

fn error_result(text: &str) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": text }] })
}

fn selected_tools(deps: &McpDeps) -> Vec<&ToolDef> {
    // A failing lookup counts as "none configured": the worst case is offering the generic readers
    // when they'd have been redundant.
    let external = (deps.read_mcp_servers)();
    select_tools(&deps.catalog, &deps.enabled_feature_ids, &external).into_iter().filter(|d| has_handler(deps, &d.name)).collect()
}

async fn call_tool(deps: &McpDeps, params: &Value) -> Value {
    let tools = selected_tools(deps);
    let name = params["name"].as_str().unwrap_or("");
    // Only a selected tool's own name ever reaches the log: a client-chosen string could be anything.
    let Some(def) = tools.iter().find(|d| d.name == name) else { return error_result(&format!("Unknown tool {name}")) };
    let started = Instant::now();
    let finish = |outcome: &str| println!("[mcp] tools/call {} {outcome} ({} ms)", def.name, started.elapsed().as_millis());
    let args = if params["arguments"].is_null() { json!({}) } else { params["arguments"].clone() };
    let checked = match validate_tool_args(def, &args, &deps.features, deps.git) {
        Ok(v) => v,
        Err(e) => {
            finish("error");
            return error_result(&e);
        }
    };
    let enabled = deps.enabled_feature_ids.clone();
    match run_handler(deps, &def.name, checked, tool_context(deps, enabled)).await {
        Ok(result) => {
            finish("ok");
            json!({ "content": [{ "type": "text", "text": cap_tool_output(&result, DEFAULT_MAX_OUTPUT_CHARS) }] })
        }
        Err(e) => {
            finish("error");
            error_result(&e)
        }
    }
}

// ---- JSON-RPC -------------------------------------------------------------------------------

/// The answer to one JSON-RPC message; `None` for a notification.
async fn handle_message(deps: &McpDeps, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let Some(method) = msg["method"].as_str().filter(|_| msg["jsonrpc"] == "2.0") else {
        // A response or garbage: nothing to answer.
        return if msg.get("result").is_some() || msg.get("error").is_some() { None } else { Some(rpc_error(-32600, "Invalid Request", id.unwrap_or(Value::Null))) };
    };
    let id = id?; // no id: a notification
    let params = &msg["params"];
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = if SUPPORTED_PROTOCOLS.contains(&asked) { asked } else { SUPPORTED_PROTOCOLS[0] };
            json!({ "protocolVersion": version, "capabilities": { "tools": {} }, "serverInfo": { "name": COMPANION_MCP_SERVER, "version": deps.version } })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": selected_tools(deps).iter().map(|d| json!({ "name": d.name, "description": d.description, "inputSchema": input_schema_for(d, &deps.features) })).collect::<Vec<_>>() }),
        "tools/call" => {
            if !params["name"].is_string() {
                return Some(rpc_error(-32602, "Invalid params: name is required", id));
            }
            call_tool(deps, params).await
        }
        _ => return Some(rpc_error(-32601, "Method not found", id)),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// The whole `POST /mcp`, once the body is in. Repeats the access check so a mounting mistake can
/// never leave this open.
pub async fn handle_mcp(req: &McpRequest<'_>, body: &[u8], deps: &McpDeps) -> McpResponse {
    if req.method != "POST" {
        return not_allowed();
    }
    if let Some(refusal) = refusal_or_500(req, deps) {
        return refusal;
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else { return McpResponse::json(400, rpc_error(-32700, "Parse error", Value::Null)) };
    match &parsed {
        Value::Array(batch) if !batch.is_empty() => {
            let mut answers = vec![];
            for m in batch {
                if let Some(a) = handle_message(deps, m).await {
                    answers.push(a);
                }
            }
            if answers.is_empty() { McpResponse { status: 202, headers: vec![], body: None } } else { McpResponse::json(200, Value::Array(answers)) }
        }
        Value::Object(_) => match handle_message(deps, &parsed).await {
            Some(a) => McpResponse::json(200, a),
            None => McpResponse { status: 202, headers: vec![], body: None },
        },
        _ => McpResponse::json(400, rpc_error(-32600, "Invalid Request", Value::Null)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_tools::tool_catalog;
    use crate::packs::{PackRegistry, Pack};

    const TOKEN: &str = "tok-123";
    const PORT: u16 = 4123;

    struct Fx {
        deps: McpDeps,
        _dir: tempfile::TempDir,
    }

    fn catalog() -> Vec<ToolDef> {
        tool_catalog(&PackRegistry::<()>::new(vec![Pack::new("p", vec![])]).unwrap()).unwrap()
    }

    use crate::providers::{GitCapability, GitHost, IssueCapability, IssueTracker, CiServer, NewPullRequest, NewSubtask, Result as PResult, ReviewerIds};
    use crate::atlassian::AtlassianError;

    fn nope<T>() -> PResult<T> {
        Err(AtlassianError::Other("not faked".into()))
    }

    /// A PR 7 that is open, 8 that is merged, 9 with no branches.
    struct FakeGit;

    #[async_trait::async_trait]
    impl GitHost for FakeGit {
        fn id(&self) -> &'static str { "fake" }
        fn capabilities(&self) -> &'static [GitCapability] { &[] }
        fn base_url(&self) -> PResult<String> { Ok("https://bb.example".into()) }
        fn has_token(&self) -> PResult<bool> { Ok(true) }
        fn pr_url(&self, project: &str, repo: &str, id: i64) -> PResult<String> { Ok(format!("https://bb.example/projects/{project}/repos/{repo}/pull-requests/{id}")) }
        async fn who_am_i(&self, _: &AuthContext) -> PResult<Option<String>> { Ok(Some("me".into())) }
        async fn get_pull_request(&self, _: &AuthContext, _: &str, _: &str, id: i64) -> PResult<Option<Value>> {
            Ok(match id {
                7 => Some(json!({ "state": "OPEN", "fromBranch": "feature/x", "toBranch": "master" })),
                8 => Some(json!({ "state": "MERGED", "fromBranch": "a", "toBranch": "b" })),
                9 => Some(json!({ "state": "OPEN" })),
                _ => None,
            })
        }
        async fn list_activities(&self, _: &AuthContext, _: &str, _: &str, _: i64) -> PResult<Vec<Value>> {
            Ok(vec![json!({ "id": 11, "text": "Please fix", "state": "OPEN", "authorSlug": "bob", "anchor": { "path": "src/a.ts", "line": 3, "lineType": "ADDED", "fileType": "TO" }, "replies": [] })])
        }
        async fn reply_to_comment(&self, _: &AuthContext, _: &str, _: &str, _: i64, _: i64, _: &str) -> PResult<Option<i64>> { nope() }
        async fn get_default_branch(&self, _: &AuthContext, _: &str, _: &str) -> PResult<Option<String>> { nope() }
        async fn list_branch_pull_requests(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> PResult<Vec<Value>> { nope() }
        async fn create_pull_request(&self, _: &AuthContext, _: &NewPullRequest) -> PResult<Value> { nope() }
        async fn get_repository_id(&self, _: &AuthContext, _: &str, _: &str) -> PResult<i64> { nope() }
        async fn get_default_reviewers(&self, _: &AuthContext, _: &str, _: &str, _: &ReviewerIds) -> PResult<Vec<String>> { nope() }
        async fn get_commit_build_status(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> PResult<Option<Value>> { nope() }
        async fn list_dashboard_pull_requests(&self, _: &AuthContext, _: &str) -> PResult<Vec<Value>> { nope() }
        async fn get_merge_status(&self, _: &AuthContext, _: &str, _: &str, _: i64) -> PResult<Value> { nope() }
    }

    struct FakeIssues;

    #[async_trait::async_trait]
    impl IssueTracker for FakeIssues {
        fn id(&self) -> &'static str { "fake" }
        fn capabilities(&self) -> &'static [IssueCapability] { &[] }
        fn base_url(&self) -> String { "https://jira.example".into() }
        fn has_token(&self) -> bool { true }
        fn issue_url(&self, key: &str) -> String { format!("https://jira.example/browse/{key}") }
        fn subtask_parent_types(&self) -> &[String] { &[] }
        async fn search_issues(&self, _: &AuthContext, _: &str, _: Option<i64>) -> PResult<Vec<Value>> { nope() }
        async fn get_issue_basics(&self, _: &AuthContext, _: &str) -> PResult<Value> { nope() }
        async fn get_issue_raw(&self, _: &AuthContext, key: &str, fields: &str) -> PResult<Value> {
            assert!(fields.contains("comment"));
            let comments: Vec<Value> = (0..25).map(|i| json!({ "author": { "name": "bob" }, "created": "2026-09-01", "body": format!("c{i}") })).collect();
            Ok(json!({ "key": key, "fields": {
                "summary": "Login loops", "status": { "name": "Open" }, "issuetype": { "name": "Bug" }, "assignee": { "displayName": "Ann", "name": "ann" },
                "components": [{ "name": "web" }, {}], "labels": ["a"], "description": "d".repeat(25_000), "comment": { "comments": comments } } }))
        }
        async fn get_issue_comments(&self, _: &AuthContext, _: &str) -> PResult<Value> { nope() }
        async fn add_comment(&self, _: &AuthContext, _: &str, _: &str) -> PResult<Option<String>> { nope() }
        async fn list_transitions(&self, _: &AuthContext, _: &str) -> PResult<Vec<crate::jira_workflow::Transition>> { nope() }
        async fn transition_issue(&self, _: &AuthContext, _: &str, _: &str) -> PResult<()> { nope() }
        async fn add_remote_link(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> PResult<()> { nope() }
        async fn get_remote_links_raw(&self, _: &AuthContext, _: &str) -> PResult<Value> { nope() }
        async fn get_linked_pull_requests_raw(&self, _: &AuthContext, _: &str) -> PResult<Value> { nope() }
        async fn create_subtask(&self, _: &AuthContext, _: &NewSubtask) -> PResult<Value> { nope() }
    }

    struct FakeCi;

    #[async_trait::async_trait]
    impl CiServer for FakeCi {
        fn id(&self) -> &'static str { "fake" }
        fn base_url(&self) -> String { "https://ci.example".into() }
        fn has_token(&self) -> bool { true }
        async fn fetch_test_report(&self, _: &AuthContext, _: &str, _: i64) -> PResult<Option<Value>> { nope() }
    }

    fn providers() -> Providers {
        Providers { git: Arc::new(FakeGit), issues: Arc::new(FakeIssues), ci: Arc::new(FakeCi) }
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let deps = McpDeps {
            config: Arc::new(|| json!({ "repos": {} })),
            home: dir.path().to_path_buf(),
            version: "9.9.9".into(),
            port: PORT,
            git: GitRemoteRules::BitbucketDc,
            get_token: Arc::new(|| Ok(Some(TOKEN.into()))),
            store: Arc::new(JobStore::new()),
            providers: providers(),
            catalog: catalog(),
            features: Arc::new(FeatureRegistry::new(vec![crate::packs::FeatureDescriptor::new("analyze-issue", "Analyze")])),
            enabled_feature_ids: vec![],
            history: None,
            start_feature_job: Arc::new(|_, _| Box::pin(async { Err("not started in tests".to_string()) })),
            read_mcp_servers: Arc::new(Vec::new),
            notifications: None,
            open_location: None,
            scope_key_for: Arc::new(|_, _| None),
            extra_handlers: HashMap::new(),
        };
        Fx { deps, _dir: dir }
    }

    fn req<'a>(method: &'a str, host: Option<&'a str>, auth: Option<&'a str>) -> McpRequest<'a> {
        McpRequest { method, host, authorization: auth }
    }

    async fn rpc(f: &Fx, body: Value) -> McpResponse {
        handle_mcp(&req("POST", Some("127.0.0.1:4123"), Some("Bearer tok-123")), body.to_string().as_bytes(), &f.deps).await
    }

    fn text(r: &McpResponse) -> Value {
        serde_json::from_str(r.body.as_ref().unwrap()["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn the_gate_checks_the_host_then_the_token_exists_then_the_bearer_and_only_for_post() {
        let f = fx();
        assert!(gate(&req("GET", None, None), &f.deps).is_none());
        let wrong_host = gate(&req("POST", Some("evil.example:4123"), Some("Bearer tok-123")), &f.deps).unwrap();
        assert_eq!(wrong_host.status, 403);
        assert!(wrong_host.body.unwrap()["error"].as_str().unwrap().contains("127.0.0.1:4123 or localhost:4123"));
        let wrong = gate(&req("POST", Some("localhost:4123"), Some("Bearer nope")), &f.deps).unwrap();
        assert_eq!((wrong.status, wrong.headers), (401, vec![("WWW-Authenticate".to_string(), "Bearer".to_string())]));
        assert!(gate(&req("POST", Some("localhost:4123"), None), &f.deps).is_some());
        assert!(gate(&req("POST", Some("127.0.0.1:4123"), Some("Bearer tok-123")), &f.deps).is_none());
    }

    #[test]
    fn an_unset_token_is_a_503_and_an_unreadable_store_a_500_that_never_leaks_the_error() {
        let mut f = fx();
        f.deps.get_token = Arc::new(|| Ok(None));
        let r = gate(&req("POST", Some("127.0.0.1:4123"), Some("Bearer ")), &f.deps).unwrap();
        assert_eq!(r.status, 503);
        f.deps.get_token = Arc::new(|| Err("keychain exploded: secret".into()));
        let r = gate(&req("POST", Some("127.0.0.1:4123"), Some("Bearer x")), &f.deps).unwrap();
        assert_eq!(r.status, 500);
        assert!(!r.body.unwrap().to_string().contains("secret"));
    }

    #[test]
    fn the_extensions_own_secret_header_is_not_even_a_parameter_here() {
        // McpRequest has no field for X-Companion-Secret: it can't be accepted by construction.
        let f = fx();
        assert_eq!(gate(&req("POST", Some("127.0.0.1:4123"), None), &f.deps).unwrap().status, 401);
        assert_eq!(not_allowed().status, 405);
        assert_eq!(not_allowed().headers, vec![("Allow".to_string(), "POST".to_string())]);
        assert_eq!(body_too_large().status, 413);
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_a_json_rpc_parse_error_and_only_after_the_access_check() {
        let f = fx();
        let r = handle_mcp(&req("POST", Some("127.0.0.1:4123"), Some("Bearer tok-123")), b"{nope", &f.deps).await;
        assert_eq!((r.status, r.body.unwrap()["error"]["code"].clone()), (400, json!(-32700)));
        let r = handle_mcp(&req("POST", Some("127.0.0.1:4123"), None), b"{nope", &f.deps).await;
        assert_eq!(r.status, 401);
        assert_eq!(handle_mcp(&req("GET", None, None), b"", &f.deps).await.status, 405);
    }

    #[tokio::test]
    async fn initialize_ping_and_notifications_follow_the_protocol() {
        let f = fx();
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })).await;
        let b = r.body.unwrap();
        assert_eq!((b["result"]["protocolVersion"].clone(), b["result"]["serverInfo"].clone()), (json!("2025-03-26"), json!({ "name": "ai-companion", "version": "9.9.9" })));
        assert_eq!(rpc(&f, json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })).await.body.unwrap()["result"], json!({}));
        let n = rpc(&f, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!((n.status, n.body), (202, None));
        let unknown = rpc(&f, json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" })).await;
        assert_eq!(unknown.body.unwrap()["error"]["code"], -32601);
        let junk = rpc(&f, json!({ "id": 4 })).await;
        assert_eq!(junk.body.unwrap()["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn tools_list_offers_only_tools_that_have_a_handler_with_plain_json_schemas() {
        let f = fx();
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
        let tools = r.body.unwrap()["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_jobs") && names.contains(&"get_job") && names.contains(&"jira_get_issue"));
        // No handler was handed in for these, so they are not advertised.
        assert!(!names.contains(&"find_similar") && !names.contains(&"get_change_risk"));
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
    }

    #[tokio::test]
    async fn a_user_configured_jira_server_makes_the_generic_jira_reader_redundant() {
        let mut f = fx();
        f.deps.read_mcp_servers = Arc::new(|| vec!["my-Jira-server".into()]);
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
        let names: Vec<String> = r.body.unwrap()["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert!(!names.contains(&"jira_get_issue".to_string()));
    }

    #[tokio::test]
    async fn list_jobs_and_get_job_read_the_store_and_trim_a_copy_of_a_long_diff() {
        let f = fx();
        let job = f.deps.store.create("analyze-issue", Map::new());
        let id = job["id"].as_str().unwrap().to_string();
        f.deps.store.update(&id, json!({ "result": { "summary": "S", "files": [{ "path": "a", "diff": "x".repeat(25_000) }] } }).as_object().unwrap().clone()).unwrap();
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_jobs", "arguments": { "featureId": "analyze-issue" } } })).await;
        let listed = text(&r);
        assert_eq!((listed[0]["id"].clone(), listed[0]["summary"].clone()), (json!(id), json!("S")));
        // A feature id that isn't one of the enabled features never reaches the handler.
        let none = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_jobs", "arguments": { "featureId": "other" } } })).await;
        assert_eq!(none.body.unwrap()["result"]["isError"], true);
        let got = text(&rpc(&f, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "get_job", "arguments": { "jobId": id } } })).await);
        let diff = got["result"]["files"][0]["diff"].as_str().unwrap();
        assert!(diff.contains("truncated: 5000 more characters"), "{}", &diff[diff.len() - 90..]);
        // The stored job is untouched.
        assert_eq!(f.deps.store.get(&id).unwrap()["result"]["files"][0]["diff"].as_str().unwrap().len(), 25_000);
        let missing = rpc(&f, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "get_job", "arguments": { "jobId": "0a1b2c3d-0000-4000-8000-000000000009" } } })).await;
        assert_eq!(missing.body.as_ref().unwrap()["result"]["isError"], true);
        assert_eq!(missing.body.unwrap()["result"]["content"][0]["text"], "No job with that id.");
    }

    #[tokio::test]
    async fn bad_arguments_and_unknown_tools_are_tool_errors_not_protocol_errors() {
        let f = fx();
        let bad = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "get_job", "arguments": { "nope": 1 } } })).await;
        let b = bad.body.unwrap();
        assert_eq!(b["result"]["isError"], true);
        assert!(b["result"]["content"][0]["text"].as_str().unwrap().contains("Unknown argument"));
        let unknown = rpc(&f, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "rm_rf", "arguments": {} } })).await;
        assert_eq!(unknown.body.unwrap()["result"]["content"][0]["text"], "Unknown tool rm_rf");
        let nameless = rpc(&f, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {} })).await;
        assert_eq!(nameless.body.unwrap()["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn history_tools_say_so_when_the_history_is_off() {
        let f = fx();
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_history", "arguments": {} } })).await;
        assert!(r.body.unwrap()["result"]["content"][0]["text"].as_str().unwrap().starts_with("Local history is off"));
    }

    #[tokio::test]
    async fn lookup_local_repo_reports_a_configured_clone_and_none_otherwise_without_writing_config() {
        let mut f = fx();
        let clone = f._dir.path().join("sample-app");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        let clone_s = clone.to_string_lossy().into_owned();
        f.deps.config = Arc::new(move || json!({ "repos": { "ACME/Sample-App": clone_s } }));
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "lookup_local_repo", "arguments": { "project": "acme", "repo": "sample-app" } } })).await;
        assert_eq!(text(&r), json!({ "path": clone.to_string_lossy(), "source": "configured" }));
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "lookup_local_repo", "arguments": { "project": "ACME", "repo": "nowhere" } } })).await;
        assert_eq!(text(&r), json!({ "path": null, "source": "none" }));
    }

    #[tokio::test]
    async fn an_extra_handler_serves_its_tool_with_the_context_and_its_error_becomes_a_tool_error() {
        let mut f = fx();
        f.deps.extra_handlers.insert("find_similar".into(), Arc::new(|args, ctx| Box::pin(async move { Ok(json!({ "got": args, "auth": ctx.auth.cookie })) })));
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
        assert!(r.body.unwrap()["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "find_similar"));
        let ok = rpc(&f, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "find_similar", "arguments": { "text": "login loop" } } })).await;
        assert_eq!(text(&ok)["auth"], Value::Null);
        f.deps.extra_handlers.insert("find_similar".into(), Arc::new(|_, _| Box::pin(async { Err("history is off".to_string()) })));
        let err = rpc(&f, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "find_similar", "arguments": { "text": "x" } } })).await;
        assert_eq!(err.body.unwrap()["result"]["content"][0]["text"], "history is off");
    }

    #[tokio::test]
    async fn a_batch_answers_each_request_and_skips_notifications() {
        let f = fx();
        let r = rpc(&f, json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }, { "jsonrpc": "2.0", "method": "notifications/initialized" }, { "jsonrpc": "2.0", "id": 2, "method": "ping" }])).await;
        assert_eq!(r.body.unwrap().as_array().unwrap().len(), 2);
        assert_eq!(rpc(&f, json!([])).await.status, 400);
    }

    #[tokio::test]
    async fn await_job_returns_when_the_job_settles_the_timeout_passes_or_it_vanishes() {
        let store = Arc::new(JobStore::new());
        assert_eq!(await_job(store.clone(), "nope".into(), Duration::from_millis(10)).await.unwrap_err(), "No job with that id.");
        let job = store.create("f", Map::new());
        let id = job["id"].as_str().unwrap().to_string();
        let still = await_job(store.clone(), id.clone(), Duration::ZERO).await.unwrap();
        assert_eq!(still["status"], "running");
        store.update(&id, json!({ "status": "awaiting-approval" }).as_object().unwrap().clone()).unwrap();
        assert_eq!(await_job(store, id, Duration::from_secs(5)).await.unwrap()["status"], "awaiting-approval");
    }

    #[test]
    fn the_pending_start_result_names_the_pr_and_says_nothing_runs_until_the_click() {
        let r = pending_start_result(&providers(), &json!({ "id": "j1" }), "ACME", "app", 7).unwrap();
        assert_eq!((r["jobId"].clone(), r["status"].clone()), (json!("j1"), json!("pending-start")));
        assert!(r["message"].as_str().unwrap().contains("nothing runs until then"));
    }

    #[tokio::test]
    async fn the_open_pull_request_check_names_what_is_wrong() {
        let p = providers();
        let a = AuthContext::default();
        assert_eq!(fetch_open_pull_request(&p, &a, "ACME", "app", 7).await.unwrap(), ("feature/x".to_string(), "master".to_string()));
        assert_eq!(fetch_open_pull_request(&p, &a, "ACME", "app", 8).await.unwrap_err(), "Pull request ACME/app #8 is merged, not open.");
        assert_eq!(fetch_open_pull_request(&p, &a, "ACME", "app", 9).await.unwrap_err(), "Bitbucket didn't report the source and target branches of ACME/app #9.");
        assert_eq!(fetch_open_pull_request(&p, &a, "ACME", "app", 1).await.unwrap_err(), "Bitbucket didn't return pull request ACME/app #1.");
    }

    #[tokio::test]
    async fn jira_get_issue_is_compact_with_capped_text_the_last_twenty_comments_and_name_fallbacks() {
        let f = fx();
        let r = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "jira_get_issue", "arguments": { "issueKey": "proj-1" } } })).await;
        let v = text(&r);
        assert_eq!((v["key"].clone(), v["status"].clone(), v["type"].clone(), v["assignee"].clone(), v["reporter"].clone()), (json!("PROJ-1"), json!("Open"), json!("Bug"), json!("Ann"), Value::Null));
        assert_eq!((v["components"].clone(), v["description"].as_str().unwrap().len(), v["comments"].as_array().unwrap().len()), (json!(["web"]), 20_000, 20));
        assert_eq!((v["comments"][0]["body"].clone(), v["comments"][0]["author"].clone()), (json!("c5"), json!("bob")));
    }

    #[tokio::test]
    async fn the_bitbucket_readers_use_the_providers_and_list_open_comments_in_the_reduced_shape() {
        let f = fx();
        let pr = rpc(&f, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "bitbucket_get_pull_request", "arguments": { "project": "ACME", "repo": "app", "prId": 7 } } })).await;
        assert_eq!(text(&pr)["fromBranch"], "feature/x");
        let c = rpc(&f, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "bitbucket_list_open_comments", "arguments": { "project": "ACME", "repo": "app", "prId": "7" } } })).await;
        let v = text(&c);
        assert_eq!((v[0]["id"].clone(), v[0]["author"].clone(), v[0]["text"].clone(), v[0]["path"].clone()), (json!(11), json!("bob"), json!("Please fix"), json!("src/a.ts")));
    }

    #[tokio::test]
    async fn a_pending_job_needs_a_scope_key_and_start_job_refuses_a_feature_that_is_not_enabled() {
        let f = fx();
        let ctx = tool_context(&f.deps, vec!["analyze-issue".into()]);
        assert_eq!((ctx.create_pending_job)("resolve-conflict", &json!({})).unwrap_err(), "Can't queue a resolve-conflict job for that payload.");
        assert_eq!((ctx.start_job)("nope".into(), json!({})).await.unwrap_err(), "Feature \"nope\" isn't enabled.");
        let mut f2 = fx();
        f2.deps.scope_key_for = Arc::new(|id, _| Some(format!("{id}:k")));
        let ctx = tool_context(&f2.deps, vec![]);
        let job = (ctx.create_pending_job)("resolve-conflict", &json!({ "a": 1 })).unwrap();
        assert_eq!((job["status"].clone(), job["scopeKey"].clone(), job["startedVia"].clone()), (json!("pending-start"), json!("resolve-conflict:k"), json!("mcp")));
    }
}
