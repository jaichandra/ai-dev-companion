//! Summarize comments (port of `features/summarize-comments/index.ts`): a short
//! summary of a Jira ticket's comment thread, written by a small fast model.
//! Read-only: it reads the comments through the tracker and runs `claude -p` with
//! no tools in a scratch directory, so the model can neither touch files nor reach
//! the network. The extension only offers it on tickets that have comments.

use super::plan::{build_prompt, normalize_comments, parse_summary, read_summary_cache, select_within_budget, write_summary_cache, DEFAULT_BUDGET_CHARS};
use crate::atlassian::AuthContext;
use crate::claude::{ClaudeRunOptions, ClaudeRunner};
use crate::claude_args::Policy;
use crate::config::{ConfigFn, DEFAULT_SUMMARIZE_COMMENTS_MODEL};
use crate::history_record::redact_secrets;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::providers::IssueTracker;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// What this feature needs from the issue tracker.
#[async_trait]
pub trait CommentsApi: Send + Sync {
    async fn get_issue_basics(&self, auth: &AuthContext, key: &str) -> Result<Value, String>;
    async fn get_issue_comments(&self, auth: &AuthContext, key: &str) -> Result<Value, String>;
}

/// The real tracker.
pub struct TrackerComments(pub Arc<dyn IssueTracker>);

#[async_trait]
impl CommentsApi for TrackerComments {
    async fn get_issue_basics(&self, auth: &AuthContext, key: &str) -> Result<Value, String> {
        self.0.get_issue_basics(auth, key).await.map_err(|e| e.to_string())
    }
    async fn get_issue_comments(&self, auth: &AuthContext, key: &str) -> Result<Value, String> {
        self.0.get_issue_comments(auth, key).await.map_err(|e| e.to_string())
    }
}

fn parse_issue_key(raw: &Value) -> Result<String, String> {
    super::super::create_jira_subtasks::plan::parse_issue_key_value(Some(raw)).map(|k| k.key).map_err(|_| {
        let shown = if raw.is_null() { String::new() } else { crate::js_text::js_to_string(raw) };
        format!("\"{shown}\" doesn't look like a Jira issue key (expected e.g. \"PROJ-34034\").")
    })
}

struct Running {
    cancel: CancellationToken,
    done: Option<JoinHandle<()>>,
}

pub struct SummarizeComments {
    store: Arc<JobStore>,
    api: Arc<dyn CommentsApi>,
    claude: Arc<dyn ClaudeRunner>,
    config: ConfigFn,
    /// The folder that holds config.json: the saved summaries live under it.
    cache_root: PathBuf,
    running: Arc<Mutex<HashMap<String, Running>>>,
}

pub fn create_summarize_comments_feature(store: Arc<JobStore>, api: Arc<dyn CommentsApi>, claude: Arc<dyn ClaudeRunner>, config: ConfigFn, cache_root: PathBuf) -> Arc<SummarizeComments> {
    Arc::new(SummarizeComments { store, api, claude, config, cache_root, running: Arc::new(Mutex::new(HashMap::new())) })
}

fn patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.update(id, v.as_object().cloned().unwrap_or_default());
}

enum Outcome {
    Done,
    Cancelled,
}

impl SummarizeComments {
    async fn run(&self, job_id: &str, issue_key: &str, force: bool, auth: &AuthContext, cancel: &CancellationToken) -> Result<Outcome, String> {
        patch(&self.store, job_id, json!({ "progress": { "stepId": "fetch", "label": "Reading comments\u{2026}" } }));
        let (basics, raw) = tokio::join!(self.api.get_issue_basics(auth, issue_key), self.api.get_issue_comments(auth, issue_key));
        let title = basics.ok().and_then(|b| b["summary"].as_str().map(String::from));
        let comments = normalize_comments(&raw?);
        if comments.is_empty() {
            return Err(format!("{issue_key} has no comments to summarize."));
        }
        // A saved summary is reused while the thread has the same number of comments.
        let saved = if force { None } else { read_summary_cache(issue_key, &self.cache_root).ok().flatten() };
        if let Some(saved) = saved.filter(|s| s.comment_count == json!(comments.len())) {
            patch(&self.store, job_id, json!({
                "status": "awaiting-approval", "progress": null,
                "data": { "issueKey": issue_key, "title": title, "summary": saved.summary, "commentCount": saved.comment_count, "omitted": saved.omitted, "fromCache": true, "completedAt": saved.completed_at },
                "result": { "summary": format!("Summarized {} comments on {issue_key}", saved.comment_count), "files": [] },
            }));
            return Ok(Outcome::Done);
        }
        let selection = select_within_budget(&comments, DEFAULT_BUDGET_CHARS);

        patch(&self.store, job_id, json!({ "progress": { "stepId": "summarize", "label": format!("Summarizing {} comments\u{2026}", comments.len()) } }));
        // A scratch directory only: the run has no tools, so nothing is read or written there.
        let dir = std::env::temp_dir().join(format!("summarize-comments-{}", crate::ids::uuid_v4()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let configured = (self.config)();
        let model = configured["summarizeComments"]["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_SUMMARIZE_COMMENTS_MODEL).to_string();
        let mut opts = ClaudeRunOptions::new(&dir.to_string_lossy(), &build_prompt(issue_key, title.as_deref(), &selection.kept, selection.omitted), Policy::NO_TOOLS);
        opts.model = Some(model);
        opts.cancel = Some(cancel.clone());
        let result = self.claude.run(opts).await;
        let _ = std::fs::remove_dir_all(&dir);
        let result = match result {
            Ok(r) => r,
            Err(e) if e.is_aborted() || cancel.is_cancelled() => return Ok(Outcome::Cancelled),
            Err(e) => return Err(e.to_string()),
        };
        if cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let summary = serde_json::to_value(parse_summary(&redact_secrets(&result.text))?).expect("a summary serializes");
        let completed_at = crate::js_date::to_iso_string(crate::job_files::now_ms() as i64).unwrap_or_default();
        let entry = json!({ "issueKey": issue_key, "commentCount": comments.len(), "omitted": selection.omitted, "summary": summary, "completedAt": completed_at });
        if let Err(e) = write_summary_cache(&entry, &self.cache_root) {
            eprintln!("[summarize-comments] could not save the summary: {e}");
        }
        patch(&self.store, job_id, json!({
            "status": "awaiting-approval", "progress": null,
            "data": { "issueKey": issue_key, "title": title, "summary": summary, "commentCount": comments.len(), "omitted": selection.omitted, "completedAt": completed_at },
            "result": { "summary": format!("Summarized {} comments on {issue_key}", comments.len()), "files": [] },
        }));
        Ok(Outcome::Done)
    }
}

#[async_trait]
impl Feature for SummarizeComments {
    fn id(&self) -> &str {
        "summarize-comments"
    }

    fn label(&self) -> &str {
        "Summarize comments"
    }

    /// Polled read-only shape (like analyze-issue): `start` returns the running job; it lands in
    /// awaiting-approval with `data.summary`.
    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let issue_key = parse_issue_key(&payload["issueKey"])?;
        let job = self.store.create("summarize-comments", Map::from_iter([("issueKey".to_string(), json!(issue_key))]));
        let id = job["id"].as_str().unwrap_or("").to_string();
        let cancel = CancellationToken::new();
        let force = payload["force"] == json!(true);
        let (this, running) = (self.clone_parts(), self.running.clone());
        let (job_id, token) = (id.clone(), cancel.clone());
        let handle = tokio::spawn(async move {
            let outcome = this.run(&job_id, &issue_key, force, &ctx.auth, &token).await;
            match outcome {
                Ok(Outcome::Done) => {}
                Ok(Outcome::Cancelled) => patch(&this.store, &job_id, json!({ "status": "rejected", "progress": null })),
                Err(e) if token.is_cancelled() => {
                    let _ = e;
                    patch(&this.store, &job_id, json!({ "status": "rejected", "progress": null }));
                }
                Err(e) => patch(&this.store, &job_id, json!({ "status": "failed", "progress": null, "error": redact_secrets(&e) })),
            }
            running.lock().unwrap().remove(&job_id);
        });
        self.running.lock().unwrap().insert(id.clone(), Running { cancel, done: Some(handle) });
        Ok(self.store.get(&id).unwrap_or(job))
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
        patch(&self.store, &id, json!({ "status": "rejecting", "progress": { "stepId": "cancel", "label": "Cancelling\u{2026}" } }));
        cancel.cancel();
        if let Some(h) = done {
            let _ = h.await;
        }
        Ok(())
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::JobStatus(format!("Cannot approve job in status \"{}\" \u{2014} summarize-comments is a read-only report", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let status = job["status"].as_str().unwrap_or("");
        if status == "awaiting-approval" || status == "failed" {
            patch(&self.store, job["id"].as_str().unwrap_or(""), json!({ "status": "rejected", "progress": null }));
            return Ok(());
        }
        Err(FeatureError::JobStatus(format!("Cannot reject job in status \"{status}\"")))
    }
}

impl SummarizeComments {
    /// A handle on the parts a spawned run needs (the feature itself is behind an `Arc` the trait
    /// method doesn't hand out).
    fn clone_parts(&self) -> Arc<SummarizeComments> {
        Arc::new(SummarizeComments { store: self.store.clone(), api: self.api.clone(), claude: self.claude.clone(), config: self.config.clone(), cache_root: self.cache_root.clone(), running: self.running.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::testing::FakeClaude;
    use crate::claude::{ClaudeError, ClaudeRunResult};
    use std::time::Duration;

    struct Api {
        basics: Result<Value, String>,
        comments: Result<Value, String>,
    }

    #[async_trait]
    impl CommentsApi for Api {
        async fn get_issue_basics(&self, _: &AuthContext, _: &str) -> Result<Value, String> {
            self.basics.clone()
        }
        async fn get_issue_comments(&self, _: &AuthContext, _: &str) -> Result<Value, String> {
            self.comments.clone()
        }
    }

    fn comments(n: usize) -> Value {
        json!({ "comments": (0..n).map(|i| json!({ "author": { "displayName": format!("User {i}") }, "created": "2026-09-01T10:00:00.000+0000", "body": format!("Comment number {i}") })).collect::<Vec<_>>() })
    }

    fn api(n: usize) -> Arc<Api> {
        Arc::new(Api { basics: Ok(json!({ "key": "PROJ-1", "summary": "Login broken" })), comments: Ok(comments(n)) })
    }

    const GOOD: &str = "{\"tldr\":\"Users loop at login\",\"decisions\":[\"Ship a fix\"],\"openQuestions\":[],\"nextSteps\":[\"Add test\"]}";

    struct Fx {
        feature: Arc<SummarizeComments>,
        store: Arc<JobStore>,
        claude: Arc<FakeClaude>,
        _dir: tempfile::TempDir,
    }

    fn fx(api: Arc<Api>, claude: FakeClaude, config: Value) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let (store, claude) = (Arc::new(JobStore::new()), Arc::new(claude));
        let feature = create_summarize_comments_feature(store.clone(), api, claude.clone(), Arc::new(move || config.clone()), dir.path().to_path_buf());
        Fx { feature, store, claude, _dir: dir }
    }

    async fn settled(store: &JobStore, id: &str) -> Value {
        for _ in 0..400 {
            let j = store.get(id).unwrap();
            if !["running", "rejecting"].contains(&j["status"].as_str().unwrap()) {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("never settled");
    }

    async fn start(f: &Fx, payload: Value) -> Value {
        let job = f.feature.start(payload, FeatureContext::default()).await.unwrap();
        settled(&f.store, job["id"].as_str().unwrap()).await
    }

    #[tokio::test]
    async fn a_thread_is_summarized_by_a_tool_less_run_and_saved_for_next_time() {
        let f = fx(api(3), FakeClaude::replying(GOOD), json!({}));
        let done = start(&f, json!({ "issueKey": "proj-1" })).await;
        assert_eq!(done["status"], "awaiting-approval");
        assert_eq!(done["result"]["summary"], "Summarized 3 comments on PROJ-1");
        assert_eq!((done["data"]["title"].clone(), done["data"]["commentCount"].clone(), done["data"]["summary"]["tldr"].clone()), (json!("Login broken"), json!(3), json!("Users loop at login")));
        assert!(done.get("progress").is_none() && done["data"].get("fromCache").is_none());
        let calls = f.claude.calls.lock().unwrap().clone();
        assert_eq!((calls.len(), calls[0].2.as_str()), (1, "noTools"));
        assert!(calls[0].1.contains("Comment number 2") && calls[0].1.contains("PROJ-1"));
        assert!(!std::path::Path::new(&calls[0].0).exists(), "the scratch folder is gone");

        // The same thread again: served from the saved summary, no model call.
        let again = start(&f, json!({ "issueKey": "PROJ-1" })).await;
        assert_eq!((again["data"]["fromCache"].clone(), again["data"]["summary"]["tldr"].clone()), (json!(true), json!("Users loop at login")));
        assert_eq!(f.claude.calls.lock().unwrap().len(), 1);
        // force bypasses it.
        start(&f, json!({ "issueKey": "PROJ-1", "force": true })).await;
        assert_eq!(f.claude.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_model_is_the_configured_one_secrets_in_the_answer_are_masked_and_a_plain_answer_is_kept_raw() {
        let seen = Arc::new(Mutex::new(vec![]));
        let s = seen.clone();
        let claude = FakeClaude::new(move |o| {
            s.lock().unwrap().push(o.model.clone());
            Ok(ClaudeRunResult { text: "Just text with Bearer abc123def and https://u:pw@host/x".into(), raw: String::new(), session_id: None, permission_denials: None })
        });
        let f = fx(api(2), claude, json!({ "summarizeComments": { "model": "claude-sonnet-5-5" } }));
        let done = start(&f, json!({ "issueKey": "PROJ-1" })).await;
        assert_eq!(*seen.lock().unwrap(), [Some("claude-sonnet-5-5".to_string())]);
        let raw = done["data"]["summary"]["raw"].as_str().unwrap();
        assert!(!raw.contains("abc123def") && !raw.contains("pw@"), "{raw}");
        let default = fx(api(2), FakeClaude::replying(GOOD), json!({}));
        let _ = start(&default, json!({ "issueKey": "PROJ-1" })).await;
    }

    #[tokio::test]
    async fn no_comments_a_bad_key_a_failing_read_and_a_failing_model_each_end_properly() {
        let f = fx(api(0), FakeClaude::replying(GOOD), json!({}));
        let none = start(&f, json!({ "issueKey": "PROJ-1" })).await;
        assert_eq!((none["status"].clone(), none["error"].clone()), (json!("failed"), json!("PROJ-1 has no comments to summarize.")));
        assert!(f.claude.calls.lock().unwrap().is_empty());
        let bad = f.feature.start(json!({ "issueKey": "nope" }), FeatureContext::default()).await.unwrap_err();
        assert_eq!(bad.to_string(), "\"nope\" doesn't look like a Jira issue key (expected e.g. \"PROJ-34034\").");
        assert!(f.feature.start(json!({}), FeatureContext::default()).await.is_err());

        let broken = Arc::new(Api { basics: Err("boom".into()), comments: Err("HTTP 500 for Bearer secret123456".into()) });
        let r = fx(broken, FakeClaude::replying(GOOD), json!({}));
        let failed = start(&r, json!({ "issueKey": "PROJ-1" })).await;
        assert_eq!(failed["status"], "failed");
        assert!(!failed["error"].as_str().unwrap().contains("secret123456"));

        let down = fx(api(2), FakeClaude::new(|_| Err(ClaudeError::Setup("claude isn't installed".into()))), json!({}));
        assert_eq!(start(&down, json!({ "issueKey": "PROJ-1" })).await["error"], "claude isn't installed");
        // The title is optional: a failing basics read doesn't fail the summary.
        let no_title = fx(Arc::new(Api { basics: Err("x".into()), comments: Ok(comments(2)) }), FakeClaude::replying(GOOD), json!({}));
        assert_eq!(start(&no_title, json!({ "issueKey": "PROJ-1" })).await["data"]["title"], Value::Null);
    }

    /// A run that only ends when it is cancelled.
    struct Hanging {
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ClaudeRunner for Hanging {
        async fn run(&self, o: ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> {
            self.started.store(true, std::sync::atomic::Ordering::SeqCst);
            o.cancel.expect("a cancel token").cancelled().await;
            Err(ClaudeError::Exec(crate::exec::ExecError::Aborted))
        }
    }

    #[tokio::test]
    async fn cancelling_a_running_summary_aborts_the_run_and_rejects_the_job() {
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (store, dir) = (Arc::new(JobStore::new()), tempfile::tempdir().unwrap());
        let feature = create_summarize_comments_feature(store.clone(), api(2), Arc::new(Hanging { started: started.clone() }), Arc::new(|| json!({})), dir.path().to_path_buf());
        let job = feature.start(json!({ "issueKey": "PROJ-1" }), FeatureContext::default()).await.unwrap();
        let id = job["id"].as_str().unwrap().to_string();
        for _ in 0..400 {
            if started.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        feature.cancel(store.get(&id).unwrap(), FeatureContext::default()).await.unwrap();
        let done = store.get(&id).unwrap();
        assert_eq!(done["status"], "rejected");
        assert!(done.get("progress").is_none());
        assert_eq!(feature.cancel(done, FeatureContext::default()).await.unwrap_err().to_string(), "Cannot cancel job in status \"rejected\"");
    }

    #[tokio::test]
    async fn the_report_is_read_only_a_finished_job_can_be_discarded_and_nothing_else() {
        let f = fx(api(2), FakeClaude::replying(GOOD), json!({}));
        let done = start(&f, json!({ "issueKey": "PROJ-1" })).await;
        let e = f.feature.approve(done.clone(), FeatureContext::default()).await.unwrap_err();
        assert_eq!(e.to_string(), "Cannot approve job in status \"awaiting-approval\" \u{2014} summarize-comments is a read-only report");
        f.feature.reject(done.clone(), FeatureContext::default()).await.unwrap();
        let rejected = f.store.get(done["id"].as_str().unwrap()).unwrap();
        assert_eq!(rejected["status"], "rejected");
        assert_eq!(f.feature.reject(rejected, FeatureContext::default()).await.unwrap_err().to_string(), "Cannot reject job in status \"rejected\"");
        assert!(f.feature.supports_cancel());
        assert_eq!((f.feature.id(), f.feature.label()), ("summarize-comments", "Summarize comments"));
    }
}
