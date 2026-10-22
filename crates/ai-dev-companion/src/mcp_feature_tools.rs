//! The MCP tools that features (and core memory/risk helpers) own, as handlers for
//! `mcp::McpDeps::extra_handlers` (the Rust form of Node's `Feature.mcpTools()`, plus the
//! `find_similar` and `get_change_risk` handlers `core/mcp.ts` closes over its deps).
//!
//! `get_digest`, `get_issue_analysis`, `analyze_issue`, `ticket_workspace`,
//! `start_resolve_conflict`, `start_address_review_comments`, `find_similar`, `get_change_risk`.
//! A handler is only advertised when its feature is enabled (the catalog decides), so registering
//! all of them is harmless. Tools of a distribution's own pack
//! are added by that distribution.

use crate::config::ConfigFn;
use crate::features::analyze_issue::plan::read_analysis_cache;
use crate::features::digest::index::{compute_digest, DigestIo};
use crate::features::ticket_workspace::index::{compute_workspace, SystemWorkspaceIo, WorkspaceEnv, WorkspaceHistory};
use crate::history_db::HistoryDb;
use crate::http::HttpTransport;
use crate::jobs::{FindSimilar, JobStore};
use crate::mcp::{fetch_open_pull_request, pending_start_result, McpHandler, McpToolContext};
use crate::mcp_background_tools::assess_push_risk_tool;
use crate::mcp_memory_tools::find_similar_tool;
use crate::prereqs::GitRemoteRules;
use crate::providers::Providers;
use crate::risk_facts::RiskFactsClient;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How long `analyze_issue` waits for the job before answering "still running".
pub const ANALYZE_WAIT: Duration = Duration::from_millis(55_000);

/// What the handlers read.
pub struct FeatureToolDeps<T: HttpTransport + 'static> {
    pub providers: Providers,
    pub store: Arc<JobStore>,
    pub config: ConfigFn,
    pub home: PathBuf,
    pub rules: GitRemoteRules,
    /// The folder that holds config.json: saved analyses live under it.
    pub cache_root: PathBuf,
    pub history: Option<Arc<HistoryDb>>,
    pub digest_io: Arc<dyn DigestIo>,
    pub find_similar: Option<Arc<dyn FindSimilar>>,
    /// The shared risk-facts client (`None`: `get_change_risk` says it can't be read).
    pub risk_facts: Option<RiskFactsClient<T>>,
}

fn handler<F, Fut>(f: F) -> McpHandler
where
    F: Fn(Value, McpToolContext) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, String>> + Send + 'static,
{
    Arc::new(move |args, ctx| Box::pin(f(args, ctx)))
}

fn text(args: &Value, name: &str) -> String {
    args[name].as_str().unwrap_or("").to_string()
}

fn pr_id(args: &Value) -> i64 {
    args["prId"].as_i64().or_else(|| args["prId"].as_f64().map(|n| n as i64)).unwrap_or(0)
}

/// `get_issue_analysis`: the saved analysis, the running job's progress, or a hint.
pub fn get_issue_analysis(store: &JobStore, cache_root: &std::path::Path, issue_key: &str) -> Value {
    if let Some(c) = read_analysis_cache(issue_key, cache_root) {
        return json!({ "issueKey": c["issueKey"], "summary": c["summary"], "repoKey": c["repoKey"], "analysis": c["analysis"], "completedAt": c["completedAt"] });
    }
    if let Some(job) = store.lookup(&format!("jira:{issue_key}"), "analyze-issue").filter(|j| j["status"] == "running") {
        return json!({ "issueKey": issue_key, "jobId": job["id"], "status": job["status"], "progress": job["progress"]["label"] });
    }
    json!({ "found": false, "hint": "Call analyze_issue to start one." })
}

/// `analyze_issue`: starts the job, waits up to `wait`, and answers with the analysis (or that it
/// is still running).
pub async fn analyze_issue(args: &Value, ctx: &McpToolContext, wait: Duration) -> Result<Value, String> {
    let issue_key = text(args, "issueKey");
    let started = (ctx.start_job)("analyze-issue".into(), json!({ "issueKey": issue_key, "force": args["force"] == json!(true), "confirmRepo": false })).await?;
    let job = (ctx.await_job)(started["id"].as_str().unwrap_or("").to_string(), wait).await?;
    if job["status"] == "running" {
        return Ok(json!({
            "jobId": job["id"], "status": "running", "progress": job["progress"]["label"],
            "hint": "Still analyzing \u{2014} call get_issue_analysis (or get_job with this jobId) in a minute.",
        }));
    }
    if job["status"] == "failed" {
        return Err(job["error"].as_str().filter(|e| !e.is_empty()).unwrap_or("The analysis failed.").to_string());
    }
    let data = &job["data"];
    if !crate::js_text::js_truthy(Some(&data["analysis"])) {
        return Err(format!("The analysis ended \"{}\" without a result.", job["status"].as_str().unwrap_or("")));
    }
    Ok(json!({ "jobId": job["id"], "issueKey": data["issueKey"], "summary": data["summary"], "repoKey": data["repoKey"], "analysis": data["analysis"] }))
}

/// `start_resolve_conflict` / `start_address_review_comments`: checks the PR now and queues a
/// "pending-start" job for the extension's Start click.
async fn queue_pr_job(providers: &Providers, ctx: &McpToolContext, feature_id: &str, args: &Value, with_branches: bool) -> Result<Value, String> {
    let (project, repo, id) = (text(args, "project"), text(args, "repo"), pr_id(args));
    let (from, to) = fetch_open_pull_request(providers, &ctx.auth, &project, &repo, id).await?;
    let payload = if with_branches { json!({ "project": project, "repo": repo, "prId": id, "sourceBranch": from, "destBranch": to }) } else { json!({ "project": project, "repo": repo, "prId": id }) };
    let job = (ctx.create_pending_job)(feature_id, &payload)?;
    pending_start_result(providers, &job, &project, &repo, id)
}

/// Every feature-owned handler, keyed by tool name.
pub fn feature_tool_handlers<T: HttpTransport + 'static>(d: FeatureToolDeps<T>) -> HashMap<String, McpHandler> {
    let d = Arc::new(d);
    let mut m: HashMap<String, McpHandler> = HashMap::new();

    let x = d.clone();
    m.insert("get_digest".into(), handler(move |_args, ctx| {
        let x = x.clone();
        async move { serde_json::to_value(compute_digest(x.digest_io.as_ref(), &ctx.auth, crate::job_files::now_ms()).await?).map_err(|e| e.to_string()) }
    }));

    let x = d.clone();
    m.insert("get_issue_analysis".into(), handler(move |args, _ctx| {
        let x = x.clone();
        async move { Ok(get_issue_analysis(&x.store, &x.cache_root, &text(&args, "issueKey"))) }
    }));

    m.insert("analyze_issue".into(), handler(|args, ctx| async move { analyze_issue(&args, &ctx, ANALYZE_WAIT).await }));

    let x = d.clone();
    m.insert("ticket_workspace".into(), handler(move |args, ctx| {
        let x = x.clone();
        async move {
            let config = (x.config)();
            let io = SystemWorkspaceIo { providers: x.providers.clone(), home: x.home.clone() };
            let history = x.history.as_deref().map(|h| h as &dyn WorkspaceHistory);
            let env = WorkspaceEnv { config: &config, home: &x.home, rules: x.rules, history, io: &io };
            compute_workspace(&env, &ctx.auth, &text(&args, "issueKey"), &|_, _| {}).await
        }
    }));

    let x = d.clone();
    m.insert("start_resolve_conflict".into(), handler(move |args, ctx| {
        let x = x.clone();
        async move { queue_pr_job(&x.providers, &ctx, "resolve-conflict", &args, true).await }
    }));

    let x = d.clone();
    m.insert("start_address_review_comments".into(), handler(move |args, ctx| {
        let x = x.clone();
        async move { queue_pr_job(&x.providers, &ctx, "address-review-comments", &args, false).await }
    }));

    let x = d.clone();
    m.insert("find_similar".into(), handler(move |args, _ctx| {
        let x = x.clone();
        async move { find_similar_tool(x.find_similar.as_deref(), &args).await }
    }));

    let x = d;
    m.insert("get_change_risk".into(), handler(move |args, _ctx| {
        let x = x.clone();
        async move {
            let url = (x.config)()["riskFacts"]["url"].as_str().map(String::from);
            assess_push_risk_tool(&args, url.as_deref(), x.risk_facts.as_ref(), crate::job_files::now_ms() as i64).await
        }
    }));
    m
}

/// An owning `FindSimilar` over the history and the LLM (`similar_search::SimilarSearch`
/// borrows both, so each call builds one).
pub struct OwnedSimilar {
    pub history: Arc<HistoryDb>,
    pub llm: crate::llm::LlmHandle,
    pub config: ConfigFn,
    pub rules: GitRemoteRules,
}

#[async_trait::async_trait]
impl FindSimilar for OwnedSimilar {
    async fn find(&self, key: Option<String>, text: Option<String>, k: Option<f64>) -> Result<Value, String> {
        let search = crate::similar_search::SimilarSearch::new(Some(self.history.as_ref()), Some(&self.llm), (self.config)(), self.rules);
        search.find(key.as_deref(), text.as_deref(), k).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlassian::{AuthContext, Client};
    use crate::auth_context::AuthHub;
    use crate::http::testing::FakeTransport;
    use crate::http::HttpResponse;
    use crate::providers::{create_providers, Deps};
    use crate::site_tokens::testing::{tokens, Fixed};
    use std::sync::Mutex;

    fn ctx(created: Arc<Mutex<Vec<(String, Value)>>>, started: Arc<Mutex<Vec<(String, Value)>>>, job: Value) -> McpToolContext {
        let job2 = job.clone();
        McpToolContext {
            auth: AuthContext::default(),
            start_job: Arc::new(move |id, payload| {
                started.lock().unwrap().push((id, payload));
                let j = job2.clone();
                Box::pin(async move { Ok(j) })
            }),
            create_pending_job: Arc::new(move |id, payload| {
                created.lock().unwrap().push((id.to_string(), payload.clone()));
                Ok(json!({ "id": "pending-1" }))
            }),
            await_job: Arc::new(move |_id, _t| {
                let j = job.clone();
                Box::pin(async move { Ok(j) })
            }),
        }
    }

    fn empty_ctx(job: Value) -> McpToolContext {
        ctx(Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![])), job)
    }

    #[tokio::test]
    async fn analyze_issue_starts_the_job_and_answers_with_the_analysis() {
        let started = Arc::new(Mutex::new(vec![]));
        let done = json!({ "id": "j1", "status": "awaiting-approval", "data": { "issueKey": "PROJ-1", "summary": "s", "repoKey": "A/b", "analysis": { "rootCause": "x" } } });
        let c = ctx(Arc::new(Mutex::new(vec![])), started.clone(), done);
        let out = analyze_issue(&json!({ "issueKey": "PROJ-1", "force": true }), &c, Duration::from_millis(1)).await.unwrap();
        assert_eq!(out, json!({ "jobId": "j1", "issueKey": "PROJ-1", "summary": "s", "repoKey": "A/b", "analysis": { "rootCause": "x" } }));
        assert_eq!(started.lock().unwrap()[0], ("analyze-issue".to_string(), json!({ "issueKey": "PROJ-1", "force": true, "confirmRepo": false })));
    }

    #[tokio::test]
    async fn analyze_issue_reports_a_running_a_failed_and_an_empty_job() {
        let running = json!({ "id": "j1", "status": "running", "progress": { "label": "Reading" } });
        let out = analyze_issue(&json!({ "issueKey": "PROJ-1" }), &empty_ctx(running), Duration::from_millis(1)).await.unwrap();
        assert_eq!((out["status"].clone(), out["progress"].clone()), (json!("running"), json!("Reading")));
        let failed = json!({ "id": "j1", "status": "failed", "error": "no repo" });
        assert_eq!(analyze_issue(&json!({ "issueKey": "PROJ-1" }), &empty_ctx(failed), Duration::from_millis(1)).await.unwrap_err(), "no repo");
        let failed_blank = json!({ "id": "j1", "status": "failed" });
        assert_eq!(analyze_issue(&json!({ "issueKey": "PROJ-1" }), &empty_ctx(failed_blank), Duration::from_millis(1)).await.unwrap_err(), "The analysis failed.");
        let empty = json!({ "id": "j1", "status": "rejected", "data": {} });
        assert_eq!(analyze_issue(&json!({ "issueKey": "PROJ-1" }), &empty_ctx(empty), Duration::from_millis(1)).await.unwrap_err(), "The analysis ended \"rejected\" without a result.");
    }

    #[test]
    fn get_issue_analysis_answers_from_the_cache_then_a_running_job_then_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::new();
        assert_eq!(get_issue_analysis(&store, dir.path(), "PROJ-1")["found"], false);
        let job = store.create_pending("analyze-issue", json!({}), Some("jira:PROJ-1"));
        store.update(job["id"].as_str().unwrap(), serde_json::Map::from_iter([("status".to_string(), json!("running"))])).unwrap();
        store.set_progress_if_running(job["id"].as_str().unwrap(), "read", "Reading");
        let out = get_issue_analysis(&store, dir.path(), "PROJ-1");
        assert_eq!((out["status"].clone(), out["progress"].clone()), (json!("running"), json!("Reading")));
    }

    /// Providers over a Bitbucket that answers every call with `reply`.
    fn providers_answering(reply: Value) -> Providers {
        let saved = vec![("github.apiToken", "t"), ("jira.apiToken", "t"), ("jenkins.apiToken", "t")];
        let t = FakeTransport::new(move |_| Ok(HttpResponse { status: 200, headers: vec![], body: reply.to_string().into_bytes() }));
        let config: Value = json!({});
        let deps = Deps { client: Arc::new(Client::new(t, Arc::new(AuthHub::new(Arc::new(Fixed(saved.clone())))))), tokens: Arc::new(tokens(saved)), config: Arc::new(move || config.clone()) };
        create_providers(deps).unwrap()
    }

    fn open_pr() -> Value {
        json!({ "number": 7, "state": "open", "title": "t", "head": { "ref": "feature/x", "sha": "abc" }, "base": { "ref": "master" } })
    }

    #[tokio::test]
    async fn the_pr_tools_check_the_pr_then_queue_a_pending_job_with_the_right_payload() {
        let providers = providers_answering(open_pr());
        let created = Arc::new(Mutex::new(vec![]));
        let c = ctx(created.clone(), Arc::new(Mutex::new(vec![])), json!({}));
        let args = json!({ "project": "ACME", "repo": "app", "prId": 7 });
        let out = queue_pr_job(&providers, &c, "resolve-conflict", &args, true).await.unwrap();
        assert_eq!((out["jobId"].clone(), out["status"].clone()), (json!("pending-1"), json!("pending-start")));
        queue_pr_job(&providers, &c, "address-review-comments", &args, false).await.unwrap();
        let made = created.lock().unwrap().clone();
        assert_eq!(made[0], ("resolve-conflict".to_string(), json!({ "project": "ACME", "repo": "app", "prId": 7, "sourceBranch": "feature/x", "destBranch": "master" })));
        assert_eq!(made[1], ("address-review-comments".to_string(), json!({ "project": "ACME", "repo": "app", "prId": 7 })));
    }

    #[tokio::test]
    async fn a_closed_pr_is_refused_before_anything_is_queued() {
        let mut pr = open_pr();
        pr["state"] = json!("closed");
        pr["merged_at"] = json!("2026-01-01T00:00:00Z");
        let providers = providers_answering(pr);
        let created = Arc::new(Mutex::new(vec![]));
        let c = ctx(created.clone(), Arc::new(Mutex::new(vec![])), json!({}));
        let e = queue_pr_job(&providers, &c, "resolve-conflict", &json!({ "project": "ACME", "repo": "app", "prId": 7 }), true).await.unwrap_err();
        assert!(e.contains("is merged, not open"), "{e}");
        assert!(created.lock().unwrap().is_empty());
    }

    #[test]
    fn every_feature_tool_has_a_handler_registered() {
        let d = FeatureToolDeps::<FakeTransport> {
            providers: providers_answering(json!({})),
            store: Arc::new(JobStore::new()),
            config: Arc::new(|| json!({})),
            home: "/home".into(),
            rules: GitRemoteRules::Github,
            cache_root: "/cache".into(),
            history: None,
            digest_io: Arc::new(crate::features::digest::index::ProviderDigestIo { providers: providers_answering(json!({})), jobs: Arc::new(JobStore::new()) }),
            find_similar: None,
            risk_facts: None,
        };
        let mut names: Vec<String> = feature_tool_handlers(d).into_keys().collect();
        names.sort();
        assert_eq!(names, ["analyze_issue", "find_similar", "get_change_risk", "get_digest", "get_issue_analysis", "start_address_review_comments", "start_resolve_conflict", "ticket_workspace"]);
    }

    #[tokio::test]
    async fn find_similar_without_history_and_change_risk_without_a_feed_answer_politely() {
        let d = FeatureToolDeps::<FakeTransport> {
            providers: providers_answering(json!({})),
            store: Arc::new(JobStore::new()),
            config: Arc::new(|| json!({})),
            home: "/home".into(),
            rules: GitRemoteRules::Github,
            cache_root: "/cache".into(),
            history: None,
            digest_io: Arc::new(crate::features::digest::index::ProviderDigestIo { providers: providers_answering(json!({})), jobs: Arc::new(JobStore::new()) }),
            find_similar: None,
            risk_facts: None,
        };
        let h = feature_tool_handlers(d);
        let sim = h["find_similar"](json!({ "key": "PROJ-1" }), empty_ctx(json!({}))).await.unwrap();
        assert_eq!(sim["enabled"], false);
        let risk = h["get_change_risk"](json!({ "files": ["a.rs"] }), empty_ctx(json!({}))).await.unwrap();
        assert_eq!(risk["skipped"], true);
    }
}
