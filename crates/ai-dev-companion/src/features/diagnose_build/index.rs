//! Diagnose a failed build (port of `features/diagnose-build/index.ts`): works out why a Jenkins
//! build failed in a headless, read-only Claude run (the analyze-issue policy: read tools and the
//! user's read-only MCP servers) and reports it as data for the panel (`data.report`): through a
//! pack's diagnose command when one is installed (its text is then put into the report's shape by a
//! second, tool-less run), otherwise through a plain diagnosis prompt. The session is recorded on
//! the job (`data.claudeSession`) so Continue in Claude Code can pick it up. The build URL comes from
//! a web page, so it is re-validated here and only the rebuilt URL is used. Nothing is posted or
//! changed.

use crate::atlassian::AuthContext;
use crate::claude::{ClaudeRunOptions, ClaudeRunner};
use crate::claude_args::Policy;
use crate::config::{ConfigFn, DEFAULT_SUMMARIZE_COMMENTS_MODEL};
use crate::diagnose_prompt::{build_report_prompt, build_structure_prompt, flaky_context, parse_build_url, parse_report, BuildRef};
use crate::features::analyze_issue::plan::analysis_run_options;
use crate::history_record::redact_secrets;
use crate::jenkins_testreport::extract_failing_cases;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::packs::Diagnose;
use crate::http::HttpTransport;
use crate::risk_facts::{Facts, RiskFactsClient, RiskFactsResult};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const FLAKY_LOOKUP_BUDGET: Duration = Duration::from_millis(8000);

/// What this feature needs from the CI server.
#[async_trait]
pub trait BuildApi: Send + Sync {
    fn base_url(&self) -> String;
    async fn fetch_test_report(&self, auth: &AuthContext, job: &str, number: i64) -> Result<Option<Value>, String>;
}

/// The real CI server.
pub struct CiBuilds(pub Arc<dyn crate::providers::CiServer>);

#[async_trait]
impl BuildApi for CiBuilds {
    fn base_url(&self) -> String {
        self.0.base_url()
    }
    async fn fetch_test_report(&self, auth: &AuthContext, job: &str, number: i64) -> Result<Option<Value>, String> {
        self.0.fetch_test_report(auth, job, number).await.map_err(|e| e.to_string())
    }
}

/// The shared risk-facts feed.
#[async_trait]
pub trait FactsSource: Send + Sync {
    async fn get(&self, url: &str) -> Option<Arc<Facts>>;
}

#[async_trait]
impl<T: HttpTransport + 'static> FactsSource for RiskFactsClient<T> {
    async fn get(&self, url: &str) -> Option<Arc<Facts>> {
        match RiskFactsClient::get(self, url).await {
            RiskFactsResult::Ok { facts, .. } => Some(facts),
            RiskFactsResult::Err { .. } => None,
        }
    }
}

pub struct DiagnoseBuildDeps {
    pub store: Arc<JobStore>,
    pub ci: Arc<dyn BuildApi>,
    pub claude: Arc<dyn ClaudeRunner>,
    pub facts: Arc<dyn FactsSource>,
    pub config: ConfigFn,
    pub home: PathBuf,
    /// The pack's richer diagnose command, asked at run time (it can become usable later).
    pub richer: Arc<dyn Fn() -> Option<Diagnose> + Send + Sync>,
    /// The packs' MCP deny list.
    pub deny_list: Vec<String>,
    /// The user's read-only MCP servers.
    pub mcp_servers: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
}

struct Running {
    cancel: CancellationToken,
    done: Option<JoinHandle<()>>,
}

pub struct DiagnoseBuild {
    d: DiagnoseBuildDeps,
    running: Arc<Mutex<HashMap<String, Running>>>,
    /// The latest job per build, so a second click on the same build reopens it instead of running
    /// Claude again.
    latest_by_build: Mutex<HashMap<String, String>>,
}

pub fn create_diagnose_build_feature(d: DiagnoseBuildDeps) -> Arc<DiagnoseBuild> {
    Arc::new(DiagnoseBuild { d, running: Arc::new(Mutex::new(HashMap::new())), latest_by_build: Mutex::new(HashMap::new()) })
}

fn patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.update(id, v.as_object().cloned().unwrap_or_default());
}

fn session(id: &str, cwd: &str) -> Value {
    json!({ "id": id, "cwd": cwd, "permissionMode": "auto" })
}

enum Outcome {
    Done,
    Cancelled,
}

/// `[^A-Za-z0-9._-]+` -> `_`.
fn safe_name(name: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    out
}

impl DiagnoseBuild {
    /// Known-flaky lines for the build's failing tests, from the shared feed. Best effort: any
    /// problem gives no lines.
    async fn flaky_lines_for(&self, auth: &AuthContext, build: &BuildRef) -> Vec<String> {
        let config = (self.d.config)();
        let Some(url) = config["riskFacts"]["url"].as_str().filter(|u| !u.is_empty()) else { return vec![] };
        if build.job_name.contains('/') {
            return vec![]; // fetch_test_report addresses a top-level job only
        }
        let Some(facts) = self.d.facts.get(url).await else { return vec![] };
        let fetched = tokio::time::timeout(FLAKY_LOOKUP_BUDGET, self.d.ci.fetch_test_report(auth, &build.job_name, build.number as i64)).await;
        let Ok(Ok(Some(report))) = fetched else { return vec![] };
        let names: Vec<String> = extract_failing_cases(&report).into_iter().map(|c| c.normalized_name).collect();
        flaky_context(Some(&facts), &names)
    }

    async fn run(&self, job_id: &str, build: &BuildRef, auth: &AuthContext, cancel: &CancellationToken) -> Result<Outcome, String> {
        let store = &self.d.store;
        patch(store, job_id, json!({ "progress": { "stepId": "collect", "label": "Reading the build\u{2026}" } }));
        let flaky_lines = self.flaky_lines_for(auth, build).await;
        if cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }

        // A pack may offer a richer way to diagnose (a platform's own command); otherwise a plain
        // read-only prompt.
        let richer = (self.d.richer)();
        let run_options = analysis_run_options(false, || (self.d.mcp_servers)());
        let cwd = crate::paths::state_dir(&self.d.home).join("sessions").join(format!("diagnose-{}-{}", safe_name(&build.job_name), build.number));
        std::fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
        let cwd = cwd.to_string_lossy().into_owned();
        let session_id = crate::ids::uuid_v4();
        // Stamped before the run so a run that fails partway still leaves a session Continue in
        // Claude Code can open.
        let _ = store.set_data(job_id, "claudeSession", session(&session_id, &cwd));

        patch(store, job_id, json!({ "progress": { "stepId": "diagnose", "label": "Diagnosing with Claude\u{2026}" } }));
        let prompt = match &richer {
            Some(r) => (r.args)(&build.build_url).into_iter().next().unwrap_or_default(),
            None => build_report_prompt(&build.build_url, &build.job_name, build.number, &flaky_lines),
        };
        let mut opts = ClaudeRunOptions::new(&cwd, &prompt, Policy::READ_ONLY);
        opts.extra_allowed_tools = run_options.extra_allowed_tools;
        opts.disallowed_tools = self.d.deny_list.clone();
        opts.session_id = Some(session_id.clone());
        opts.cancel = Some(cancel.clone());
        let progress_store = store.clone();
        let progress_id = job_id.to_string();
        opts.on_progress = Some(Arc::new(move |label: &str| progress_store.set_progress_if_running(&progress_id, "diagnose", label)));
        let result = match self.d.claude.run(opts).await {
            Ok(r) => r,
            Err(e) if e.is_aborted() || cancel.is_cancelled() => return Ok(Outcome::Cancelled),
            Err(e) => return Err(e.to_string()),
        };
        if cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }

        let mut text = result.text.clone();
        if richer.is_some() {
            // The command writes its own report; put it into the panel's shape with a small
            // tool-less run.
            patch(store, job_id, json!({ "progress": { "stepId": "format", "label": "Preparing the report\u{2026}" } }));
            let scratch = std::env::temp_dir().join(format!("diagnose-build-{}", crate::ids::uuid_v4()));
            std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
            let configured = (self.d.config)();
            let model = configured["summarizeComments"]["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_SUMMARIZE_COMMENTS_MODEL).to_string();
            let mut o = ClaudeRunOptions::new(&scratch.to_string_lossy(), &build_structure_prompt(&redact_secrets(&text)), Policy::NO_TOOLS);
            o.model = Some(model);
            o.cancel = Some(cancel.clone());
            let structured = self.d.claude.run(o).await;
            let _ = std::fs::remove_dir_all(&scratch);
            match structured {
                Ok(r) => text = r.text,
                Err(e) if e.is_aborted() || cancel.is_cancelled() => return Ok(Outcome::Cancelled),
                Err(e) => return Err(e.to_string()),
            }
            if cancel.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
        }
        let report = serde_json::to_value(parse_report(&redact_secrets(&text))?).expect("a report serializes");
        let final_id = result.session_id.clone().unwrap_or(session_id);
        let completed_at = crate::js_date::to_iso_string(crate::job_files::now_ms() as i64).unwrap_or_default();
        let _ = store.patch_data(
            job_id,
            Map::from_iter([("claudeSession".to_string(), session(&final_id, &cwd)), ("report".to_string(), report), ("flakyLines".to_string(), json!(flaky_lines)), ("completedAt".to_string(), json!(completed_at))]),
        );
        patch(store, job_id, json!({
            "status": "awaiting-approval", "progress": null,
            "result": { "summary": format!("Diagnosed {} #{}", build.job_name, build.number), "files": [] },
        }));
        Ok(Outcome::Done)
    }
}

#[async_trait]
impl Feature for DiagnoseBuild {
    fn id(&self) -> &str {
        "diagnose-build"
    }

    fn label(&self) -> &str {
        "Diagnose failed builds"
    }

    /// Polled read-only shape (like summarize-comments): `start` returns the running job; it lands
    /// in awaiting-approval with `data.report`. A second start for the same build returns the job
    /// already running or finished unless the payload says `force`.
    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let build = payload["buildUrl"].as_str().and_then(|u| parse_build_url(u, &self.d.ci.base_url()));
        let Some(build) = build else {
            let job = self.d.store.create("diagnose-build", Map::new());
            let id = job["id"].as_str().unwrap_or("").to_string();
            patch(&self.d.store, &id, json!({ "status": "failed", "error": "That doesn't look like a build page of your configured Jenkins." }));
            return Ok(self.d.store.get(&id).unwrap_or(job));
        };
        let earlier = self.latest_by_build.lock().unwrap().get(&build.build_url).and_then(|id| self.d.store.get(id));
        if payload["force"] != json!(true) {
            if let Some(e) = earlier.filter(|e| matches!(e["status"].as_str(), Some("running" | "awaiting-approval"))) {
                return Ok(e);
            }
        }
        let job = self.d.store.create("diagnose-build", Map::from_iter([("buildUrl".to_string(), json!(build.build_url)), ("jobName".to_string(), json!(build.job_name)), ("number".to_string(), json!(build.number))]));
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.latest_by_build.lock().unwrap().insert(build.build_url.clone(), id.clone());
        let cancel = CancellationToken::new();
        let (this, running) = (self.parts(), self.running.clone());
        let (job_id, token) = (id.clone(), cancel.clone());
        let handle = tokio::spawn(async move {
            match this.run(&job_id, &build, &ctx.auth, &token).await {
                Ok(Outcome::Done) => {}
                Ok(Outcome::Cancelled) => patch(&this.d.store, &job_id, json!({ "status": "rejected", "progress": null })),
                Err(_) if token.is_cancelled() => patch(&this.d.store, &job_id, json!({ "status": "rejected", "progress": null })),
                Err(e) => patch(&this.d.store, &job_id, json!({ "status": "failed", "progress": null, "error": redact_secrets(&e) })),
            }
            running.lock().unwrap().remove(&job_id);
        });
        self.running.lock().unwrap().insert(id, Running { cancel, done: Some(handle) });
        Ok(job)
    }

    fn supports_cancel(&self) -> bool {
        true
    }

    async fn cancel(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let id = job["id"].as_str().unwrap_or("").to_string();
        let status = job["status"].as_str().unwrap_or("").to_string();
        let taken = {
            let mut running = self.running.lock().unwrap();
            match running.get_mut(&id) {
                Some(r) if status == "running" => Some((r.cancel.clone(), r.done.take())),
                _ => None,
            }
        };
        let Some((cancel, done)) = taken else { return Err(FeatureError::Other(format!("Cannot cancel job in status \"{status}\""))) };
        patch(&self.d.store, &id, json!({ "status": "rejecting", "progress": { "stepId": "cancel", "label": "Cancelling\u{2026}" } }));
        cancel.cancel();
        if let Some(h) = done {
            let _ = h.await;
        }
        Ok(())
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::JobStatus(format!("Cannot approve job in status \"{}\" \u{2014} diagnose-build is a read-only report", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let status = job["status"].as_str().unwrap_or("");
        if status == "awaiting-approval" || status == "failed" {
            patch(&self.d.store, job["id"].as_str().unwrap_or(""), json!({ "status": "rejected", "progress": null }));
            return Ok(());
        }
        Err(FeatureError::JobStatus(format!("Cannot reject job in status \"{status}\"")))
    }
}

impl DiagnoseBuild {
    /// A handle on the parts a spawned run needs.
    fn parts(&self) -> Arc<DiagnoseBuild> {
        let d = &self.d;
        Arc::new(DiagnoseBuild {
            d: DiagnoseBuildDeps { store: d.store.clone(), ci: d.ci.clone(), claude: d.claude.clone(), facts: d.facts.clone(), config: d.config.clone(), home: d.home.clone(), richer: d.richer.clone(), deny_list: d.deny_list.clone(), mcp_servers: d.mcp_servers.clone() },
            running: self.running.clone(),
            latest_by_build: Mutex::new(HashMap::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::testing::FakeClaude;
    use crate::claude::ClaudeError;

    const BASE: &str = "https://jenkins.example";
    const URL: &str = "https://jenkins.example/job/deploy/42/";
    const GOOD: &str = "{\"tldr\":\"Disk full on the agent\",\"category\":\"infra\",\"causes\":[],\"suspects\":[],\"failingTests\":[],\"stages\":[],\"nextSteps\":[\"Free space\"]}";

    struct Ci;

    #[async_trait]
    impl BuildApi for Ci {
        fn base_url(&self) -> String {
            BASE.into()
        }
        async fn fetch_test_report(&self, _: &AuthContext, _: &str, _: i64) -> Result<Option<Value>, String> {
            Ok(None)
        }
    }

    struct NoFacts;

    #[async_trait]
    impl FactsSource for NoFacts {
        async fn get(&self, _: &str) -> Option<Arc<Facts>> {
            None
        }
    }

    struct Fx {
        feature: Arc<DiagnoseBuild>,
        store: Arc<JobStore>,
        claude: Arc<FakeClaude>,
        _dir: tempfile::TempDir,
    }

    fn fx(claude: FakeClaude, richer: Option<Diagnose>) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let (store, claude) = (Arc::new(JobStore::new()), Arc::new(claude));
        let feature = create_diagnose_build_feature(DiagnoseBuildDeps {
            store: store.clone(),
            ci: Arc::new(Ci),
            claude: claude.clone(),
            facts: Arc::new(NoFacts),
            config: Arc::new(|| json!({})),
            home: dir.path().to_path_buf(),
            richer: Arc::new(move || richer.clone()),
            deny_list: vec!["mcp__x__delete".into()],
            mcp_servers: Arc::new(|| vec!["jira".into()]),
        });
        Fx { feature, store, claude, _dir: dir }
    }

    async fn settled(store: &JobStore, id: &str) -> Value {
        for _ in 0..400 {
            let j = store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the diagnosis never finished");
    }

    async fn start(f: &Fx, payload: Value) -> Value {
        f.feature.start(payload, FeatureContext::default()).await.unwrap()
    }

    #[tokio::test]
    async fn a_page_that_is_not_a_build_of_the_configured_jenkins_fails_at_once_without_running_claude() {
        let f = fx(FakeClaude::replying(GOOD), None);
        for payload in [json!({}), json!({ "buildUrl": 3 }), json!({ "buildUrl": "https://evil.example/job/deploy/42/" })] {
            let job = start(&f, payload).await;
            assert_eq!(job["status"], "failed");
            assert_eq!(job["error"], "That doesn't look like a build page of your configured Jenkins.");
        }
        assert!(f.claude.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_plain_diagnosis_runs_read_only_records_the_session_and_lands_awaiting_approval() {
        let f = fx(FakeClaude::replying(GOOD), None);
        let job = start(&f, json!({ "buildUrl": URL })).await;
        assert_eq!((job["data"]["jobName"].clone(), job["data"]["number"].clone()), (json!("deploy"), json!(42)));
        let done = settled(&f.store, job["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "awaiting-approval", "{done}");
        assert_eq!(done["data"]["report"]["tldr"], "Disk full on the agent");
        assert_eq!(done["data"]["claudeSession"]["permissionMode"], "auto");
        assert!(done["data"]["claudeSession"]["cwd"].as_str().unwrap().ends_with("sessions/diagnose-deploy-42"));
        assert_eq!(done["result"]["summary"], "Diagnosed deploy #42");
        let calls = f.claude.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, "readOnly");
    }

    #[tokio::test]
    async fn a_second_click_on_the_same_build_reopens_the_job_unless_forced() {
        let f = fx(FakeClaude::replying(GOOD), None);
        let first = start(&f, json!({ "buildUrl": URL })).await;
        settled(&f.store, first["id"].as_str().unwrap()).await;
        let again = start(&f, json!({ "buildUrl": URL })).await;
        assert_eq!(again["id"], first["id"]);
        let forced = start(&f, json!({ "buildUrl": URL, "force": true })).await;
        assert_ne!(forced["id"], first["id"]);
        settled(&f.store, forced["id"].as_str().unwrap()).await;
        assert_eq!(f.claude.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_packs_command_is_run_first_and_its_text_is_put_into_shape_by_a_tool_less_run() {
        let richer = Diagnose { available: Arc::new(|| true), args: Arc::new(|u| vec![format!("/build-fix {u}")]), summary: Arc::new(|_| "s".into()) };
        let f = fx(FakeClaude::new(|o| {
            let text = if o.policy.name == "noTools" { GOOD } else { "free-form report with token=abc" };
            Ok(crate::claude::ClaudeRunResult { text: text.into(), raw: String::new(), session_id: Some("sess-1".into()), permission_denials: None })
        }), Some(richer));
        let job = start(&f, json!({ "buildUrl": URL })).await;
        let done = settled(&f.store, job["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "awaiting-approval", "{done}");
        let calls = f.claude.calls.lock().unwrap().clone();
        assert_eq!(calls[0].1, format!("/build-fix {URL}"));
        assert_eq!(calls[1].2, "noTools");
        assert_eq!(done["data"]["claudeSession"]["id"], "sess-1");
    }

    #[tokio::test]
    async fn a_failed_run_is_a_failed_job_with_the_error_redacted() {
        let f = fx(FakeClaude::new(|_| Err(ClaudeError::Setup("boom".into()))), None);
        let job = start(&f, json!({ "buildUrl": URL })).await;
        let done = settled(&f.store, job["id"].as_str().unwrap()).await;
        assert_eq!((done["status"].clone(), done["error"].clone()), (json!("failed"), json!("boom")));
    }

    #[tokio::test]
    async fn the_report_is_read_only_and_a_finished_diagnosis_can_be_discarded() {
        let f = fx(FakeClaude::replying(GOOD), None);
        let job = start(&f, json!({ "buildUrl": URL })).await;
        let done = settled(&f.store, job["id"].as_str().unwrap()).await;
        assert!(f.feature.approve(done.clone(), FeatureContext::default()).await.unwrap_err().to_string().contains("read-only report"));
        assert!(f.feature.cancel(done.clone(), FeatureContext::default()).await.is_err());
        f.feature.reject(done.clone(), FeatureContext::default()).await.unwrap();
        let rejected = f.store.get(done["id"].as_str().unwrap()).unwrap();
        assert_eq!(rejected["status"], "rejected");
        assert!(f.feature.reject(rejected, FeatureContext::default()).await.is_err());
    }

    #[test]
    fn the_session_folder_name_keeps_only_safe_characters() {
        assert_eq!(safe_name("team/repo build"), "team_repo_build");
        assert_eq!(safe_name("a.b-c_d"), "a.b-c_d");
    }
}
