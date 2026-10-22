//! Morning digest (port of `features/digest/index.ts`): your PRs (conflicts,
//! builds, approvals, needs-work), PRs waiting on your review, your open tickets
//! and what the watchers pre-warmed, as one read-only view. A ✨ row on every
//! covered page runs it on a click; the `get_digest` MCP tool and `companion
//! digest` compute it directly; the scheduler posts it to the inbox on weekdays
//! at `digest.time`. Every source is best effort: one that fails becomes a
//! "Couldn't check" line, never a failed digest.

use super::plan::{build_digest, digest_notification, Digest, DigestInput};
use crate::atlassian::{AtlassianError, AuthContext};
use crate::config::ConfigFn;
use crate::guard::link_guard::link_on_hosts_value;
use crate::history_record::redact_secrets;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::js_text::slice_utf16;
use crate::providers::Providers;
use crate::schedule::local_day_key;
use crate::scheduler::{task_fn, TaskFn, TaskResult};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const MAX_MY_PRS: usize = 10;
const PREWARMED_MAX_AGE_MS: f64 = 3.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const OPEN_TICKETS_JQL: &str = "assignee = currentUser() AND statusCategory != Done AND issuetype not in subTaskIssueTypes() ORDER BY priority DESC, updated DESC";

/// Everything the digest reads.
#[async_trait]
pub trait DigestIo: Send + Sync {
    async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>, AtlassianError>;
    /// `{conflicted}` of the PR's merge status.
    async fn get_merge_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Value, AtlassianError>;
    async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>, AtlassianError>;
    async fn search_issues(&self, auth: &AuthContext, jql: &str) -> Result<Vec<Value>, AtlassianError>;
    fn jobs(&self) -> Result<Vec<Value>, String>;
    /// The git host's and the tracker's base URLs: a digest link may only point at these hosts.
    fn base_urls(&self) -> Vec<String>;
}

/// The real io: the providers and the job store.
pub struct ProviderDigestIo {
    pub providers: Providers,
    pub jobs: Arc<JobStore>,
}

#[async_trait]
impl DigestIo for ProviderDigestIo {
    async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>, AtlassianError> {
        self.providers.git.list_dashboard_pull_requests(auth, role).await
    }
    async fn get_merge_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Value, AtlassianError> {
        self.providers.git.get_merge_status(auth, project, repo, pr_id).await
    }
    async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>, AtlassianError> {
        self.providers.git.get_commit_build_status(auth, project, repo, sha).await
    }
    async fn search_issues(&self, auth: &AuthContext, jql: &str) -> Result<Vec<Value>, AtlassianError> {
        self.providers.issues.search_issues(auth, jql, Some(20)).await
    }
    fn jobs(&self) -> Result<Vec<Value>, String> {
        Ok(self.jobs.list())
    }
    fn base_urls(&self) -> Vec<String> {
        self.providers.git.base_url().into_iter().chain(Some(self.providers.issues.base_url())).collect()
    }
}

fn why(err: &AtlassianError) -> String {
    match err {
        AtlassianError::AuthSetup(_) => "no sign-in (open the site in Chrome, or save a token in \u{2699} Settings)".into(),
        AtlassianError::Status { status, .. } => format!("HTTP {status}"),
        other => slice_utf16(&redact_secrets(&other.to_string()), 160),
    }
}

/// Collects every source and builds the digest. Never fails for a source (only for the job list,
/// which is local).
pub async fn compute_digest(io: &dyn DigestIo, auth: &AuthContext, now: f64) -> Result<Digest, String> {
    // The digest's links open on a click: https on the configured git host or tracker host only.
    let hosts = io.base_urls();
    let safe = |mut item: Value| {
        let url = link_on_hosts_value(item.get("url"), &hosts);
        item["url"] = url.map_or(Value::Null, Value::String);
        item
    };
    let mut notes: Vec<String> = vec![];
    let mut my_prs: Vec<Value> = vec![];
    match io.list_dashboard_pull_requests(auth, "AUTHOR").await {
        Ok(list) => {
            let (mut merge_failed, mut build_failed) = (0, 0);
            for pr in list.into_iter().take(MAX_MY_PRS) {
                let (project, repo, id) = (pr["project"].as_str().unwrap_or("").to_string(), pr["repo"].as_str().unwrap_or("").to_string(), pr["id"].as_i64().unwrap_or(0));
                let conflicted = match io.get_merge_status(auth, &project, &repo, id).await {
                    Ok(m) => m["conflicted"].clone(),
                    Err(_) => {
                        merge_failed += 1;
                        Value::Null
                    }
                };
                let build = match pr["fromSha"].as_str().filter(|s| !s.is_empty()) {
                    Some(sha) => match io.get_commit_build_status(auth, &project, &repo, sha).await {
                        Ok(b) => b.unwrap_or(Value::Null),
                        Err(_) => {
                            build_failed += 1;
                            Value::Null
                        }
                    },
                    None => Value::Null,
                };
                my_prs.push(json!({ "pr": safe(pr), "conflicted": conflicted, "build": build }));
            }
            if merge_failed > 0 {
                notes.push(format!("Couldn't check {merge_failed} of your PRs for conflicts."));
            }
            if build_failed > 0 {
                notes.push(format!("Couldn't read the builds of {build_failed} of your PRs."));
            }
        }
        Err(e) => notes.push(format!("Couldn't read your pull requests: {}.", why(&e))),
    }
    let review_prs = match io.list_dashboard_pull_requests(auth, "REVIEWER").await {
        Ok(l) => l.into_iter().map(&safe).collect(),
        Err(e) => {
            notes.push(format!("Couldn't read your review queue: {}.", why(&e)));
            vec![]
        }
    };
    let tickets = match io.search_issues(auth, OPEN_TICKETS_JQL).await {
        Ok(l) => l.into_iter().map(&safe).collect(),
        Err(e) => {
            notes.push(format!("Couldn't read your Jira tickets: {}.", why(&e)));
            vec![]
        }
    };
    let prewarmed: Vec<Value> = io
        .jobs()?
        .iter()
        .filter(|j| j["startedVia"] == "watcher" && matches!(j["status"].as_str(), Some("awaiting-approval" | "failed")) && now - j["createdAt"].as_f64().unwrap_or(0.0) < PREWARMED_MAX_AGE_MS)
        .map(|j| {
            let mut m = Map::new();
            m.insert("featureId".into(), j["featureId"].clone());
            m.insert("status".into(), j["status"].clone());
            m.insert("scopeKey".into(), j["scopeKey"].clone());
            m.insert("summary".into(), j["result"]["summary"].clone());
            m.insert("error".into(), j["error"].as_str().map_or_else(|| j["error"].clone(), |e| json!(redact_secrets(e))));
            Value::Object(m)
        })
        .collect();
    Ok(build_digest(&DigestInput { now, my_prs, review_prs, tickets, prewarmed, notes }))
}

/// The inbox, as the digest task uses it.
pub trait DigestInbox: Send + Sync {
    fn add(&self, item: &Value) -> Result<(), String>;
    fn absorb_pending(&self) -> Vec<String>;
}

/// The scheduler's weekday digest: posted to the inbox (quiet-hours items ride along).
pub fn create_digest_task(io: Arc<dyn DigestIo>, inbox: Arc<dyn DigestInbox>) -> TaskFn {
    task_fn(move |ctx| {
        let (io, inbox) = (io.clone(), inbox.clone());
        async move {
            let now = ctx.now();
            let digest = compute_digest(io.as_ref(), &AuthContext::default(), now).await?;
            let absorbed = inbox.absorb_pending().len();
            inbox.add(&digest_notification(&digest, &local_day_key(now)))?;
            let mut r = TaskResult::ok();
            if let Value::Object(counts) = serde_json::to_value(&digest.counts).expect("counts serialize") {
                r.metrics = counts;
            }
            r = r.metric("absorbed", json!(absorbed)).metric("notes", json!(digest.sections.iter().find(|s| s.id == "notes").map_or(0, |s| s.items.len())));
            Ok(r)
        }
    })
}

pub struct DigestFeature {
    store: Arc<JobStore>,
    io: Arc<dyn DigestIo>,
    #[allow(dead_code)]
    config: ConfigFn,
}

pub fn create_digest_feature(config: ConfigFn, store: Arc<JobStore>, io: Arc<dyn DigestIo>) -> Arc<DigestFeature> {
    Arc::new(DigestFeature { store, io, config })
}

impl DigestFeature {
    /// What the `get_digest` MCP tool and `companion digest` return.
    pub async fn get_digest(&self, auth: &AuthContext) -> Result<Value, String> {
        compute_digest(self.io.as_ref(), auth, crate::job_files::now_ms()).await.map(|d| serde_json::to_value(d).expect("digest serializes"))
    }
}

fn update(store: &JobStore, id: &str, patch: Value) {
    let _ = store.update(id, patch.as_object().cloned().unwrap_or_default());
}

#[async_trait]
impl Feature for DigestFeature {
    fn id(&self) -> &str {
        "digest"
    }

    fn label(&self) -> &str {
        "Morning digest"
    }

    /// Polled read-only shape (like the ticket workspace).
    async fn start(&self, _payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let job = self.store.create("digest", Map::new());
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.store.set_progress_if_running(&id, "collect", "Reading your PRs, reviews and tickets\u{2026}");
        let (store, io, auth) = (self.store.clone(), self.io.clone(), ctx.auth);
        let job_id = id.clone();
        tokio::spawn(async move {
            match compute_digest(io.as_ref(), &auth, crate::job_files::now_ms()).await {
                Ok(digest) => {
                    let headline = digest.headline.clone();
                    let value = serde_json::to_value(&digest).expect("digest serializes");
                    update(&store, &job_id, json!({ "status": "awaiting-approval", "progress": null, "data": { "digest": value }, "result": { "summary": headline, "files": [] } }));
                }
                Err(e) => update(&store, &job_id, json!({ "status": "failed", "progress": null, "error": redact_secrets(&e) })),
            }
        });
        Ok(self.store.get(&id).unwrap_or(job))
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::JobStatus(format!("Cannot approve job in status \"{}\" \u{2014} the digest is a read-only report", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let status = job["status"].as_str().unwrap_or("");
        if status == "awaiting-approval" || status == "failed" {
            update(&self.store, job["id"].as_str().unwrap_or(""), json!({ "status": "rejected", "progress": null }));
            return Ok(());
        }
        Err(FeatureError::JobStatus(format!("Cannot reject job in status \"{status}\"")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{ClaudeRunGrant, TaskContext, TaskHost};
    use std::sync::Mutex;

    const NOW: f64 = 1790670600000.0; // 2026-09-29T08:30:00Z

    fn pr(id: i64, over: Value) -> Value {
        let mut p = json!({
            "id": id, "title": format!("PR {id}"), "state": "OPEN", "project": "CI", "repo": "sample-app",
            "fromSha": format!("sha{id}000"), "url": format!("https://bb.example/projects/CI/repos/sample-app/pull-requests/{id}"),
            "approvals": 0, "reviewers": [],
        });
        for (k, v) in over.as_object().into_iter().flatten() {
            p[k] = v.clone();
        }
        p
    }

    type Calls = Arc<Mutex<Vec<Vec<String>>>>;
    type PrFn = Box<dyn Fn(&str) -> Result<Vec<Value>, AtlassianError> + Send + Sync>;
    type MergeFn = Box<dyn Fn(i64) -> Result<Value, AtlassianError> + Send + Sync>;
    type BuildFn = Box<dyn Fn(&str) -> Result<Option<Value>, AtlassianError> + Send + Sync>;
    type IssuesFn = Box<dyn Fn() -> Result<Vec<Value>, AtlassianError> + Send + Sync>;
    type JobsFn = Box<dyn Fn() -> Result<Vec<Value>, String> + Send + Sync>;

    struct FakeIo {
        calls: Calls,
        prs: PrFn,
        merge: MergeFn,
        build: BuildFn,
        issues: IssuesFn,
        jobs: JobsFn,
    }

    fn default_jobs() -> Result<Vec<Value>, String> {
        Ok(vec![
            json!({ "id": "a", "featureId": "resolve-conflict", "status": "awaiting-approval", "startedVia": "watcher", "scopeKey": "bitbucket:CI/sample-app#1", "createdAt": NOW - 1000.0, "result": { "summary": "Resolved 1 file" } }),
            json!({ "id": "b", "featureId": "resolve-conflict", "status": "awaiting-approval", "startedVia": "extension", "createdAt": NOW }),
            json!({ "id": "c", "featureId": "analyze-issue", "status": "awaiting-approval", "startedVia": "watcher", "createdAt": NOW - 5.0 * 24.0 * 3600.0 * 1000.0 }),
        ])
    }

    fn fake() -> FakeIo {
        FakeIo {
            calls: Arc::new(Mutex::new(vec![])),
            prs: Box::new(|role| Ok(if role == "AUTHOR" { vec![pr(1, json!({})), pr(2, json!({ "approvals": 1 }))] } else { vec![pr(9, json!({ "authorSlug": "bob" }))] })),
            merge: Box::new(|id| Ok(json!({ "conflicted": id == 1 }))),
            build: Box::new(|sha| Ok(Some(json!({ "state": if sha == "sha1000" { "FAILED" } else { "SUCCESSFUL" }, "counts": {}, "builds": [] })))),
            issues: Box::new(|| Ok(vec![json!({ "key": "PROJ-7", "summary": "Login broken", "status": "Open", "priority": "High", "url": "https://jira.example/browse/PROJ-7" })])),
            jobs: Box::new(default_jobs),
        }
    }

    #[async_trait]
    impl DigestIo for FakeIo {
        async fn list_dashboard_pull_requests(&self, _: &AuthContext, role: &str) -> Result<Vec<Value>, AtlassianError> {
            self.calls.lock().unwrap().push(vec!["dashboard".into(), role.into()]);
            (self.prs)(role)
        }
        async fn get_merge_status(&self, _: &AuthContext, _: &str, _: &str, id: i64) -> Result<Value, AtlassianError> {
            (self.merge)(id)
        }
        async fn get_commit_build_status(&self, _: &AuthContext, _: &str, _: &str, sha: &str) -> Result<Option<Value>, AtlassianError> {
            (self.build)(sha)
        }
        async fn search_issues(&self, _: &AuthContext, jql: &str) -> Result<Vec<Value>, AtlassianError> {
            self.calls.lock().unwrap().push(vec!["search".into(), jql.into()]);
            (self.issues)()
        }
        fn jobs(&self) -> Result<Vec<Value>, String> {
            (self.jobs)()
        }
        fn base_urls(&self) -> Vec<String> {
            vec!["https://bb.example".into(), "https://jira.example".into()]
        }
    }

    fn texts(d: &Digest, section: &str) -> Vec<String> {
        d.sections.iter().find(|s| s.id == section).map(|s| s.items.iter().map(|i| i.text.clone()).collect()).unwrap_or_default()
    }

    #[tokio::test]
    async fn the_digest_combines_your_prs_your_review_queue_your_tickets_and_the_watchers_results() {
        let f = fake();
        let d = compute_digest(&f, &AuthContext::default(), NOW).await.unwrap();
        assert_eq!(d.headline, "Needs you: 1 conflict, 1 failing build, 1 PR to review, 1 ready for you.");
        assert_eq!(d.sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["my-prs", "to-review", "tickets", "ready"]);
        assert_eq!(d.sections[0].items[0].text, "CI/sample-app #1 PR 1 \u{2014} conflicts, build failing");
        assert_eq!(d.sections[3].items.len(), 1, "only watcher jobs from the last 3 days");
        let calls = f.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| c[0] == "dashboard").map(|c| c[1].clone()).collect::<Vec<_>>(), ["AUTHOR", "REVIEWER"]);
        assert!(calls.iter().find(|c| c[0] == "search").unwrap()[1].starts_with("assignee = currentUser() AND statusCategory != Done"));
    }

    #[tokio::test]
    async fn a_source_that_fails_becomes_a_note_never_a_failed_digest() {
        let mut f = fake();
        f.prs = Box::new(|_| Err(AtlassianError::AuthSetup("no session".into())));
        f.merge = Box::new(|_| Err(AtlassianError::Other("x".into())));
        let d = compute_digest(&f, &AuthContext::default(), NOW).await.unwrap();
        assert_eq!(texts(&d, "notes"), [
            "Couldn't read your pull requests: no sign-in (open the site in Chrome, or save a token in \u{2699} Settings).",
            "Couldn't read your review queue: no sign-in (open the site in Chrome, or save a token in \u{2699} Settings).",
        ]);
    }

    struct Inbox {
        added: Mutex<Vec<Value>>,
    }
    impl DigestInbox for Inbox {
        fn add(&self, item: &Value) -> Result<(), String> {
            self.added.lock().unwrap().push(item.clone());
            Ok(())
        }
        fn absorb_pending(&self) -> Vec<String> {
            vec!["x".into(), "y".into()]
        }
    }

    struct Host;
    impl TaskHost for Host {
        fn now(&self) -> f64 {
            NOW
        }
        fn request_claude_run(&self) -> ClaudeRunGrant {
            unreachable!()
        }
        fn refund_claude_run(&self) {}
        fn track_background_job(&self, _: &str) {}
        fn seen(&self, _: &str) -> Map<String, Value> {
            Map::new()
        }
        fn set_seen(&self, _: &str, _: Map<String, Value>) {}
    }

    #[tokio::test]
    async fn the_scheduled_digest_posts_one_inbox_item_and_takes_over_what_quiet_hours_held() {
        let inbox = Arc::new(Inbox { added: Mutex::new(vec![]) });
        let task = create_digest_task(Arc::new(fake()), inbox.clone());
        let report = task(TaskContext::new(Arc::new(Host), false)).await.unwrap();
        let added = inbox.added.lock().unwrap().clone();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["kind"], "digest");
        assert!(added[0]["key"].as_str().unwrap().starts_with("digest:2026-09-29"), "{}", added[0]["key"]);
        assert_eq!((report.metrics["absorbed"].clone(), report.metrics["conflicts"].clone()), (json!(2), json!(1)));
    }

    fn feature(io: FakeIo) -> (Arc<DigestFeature>, Arc<JobStore>) {
        let store = Arc::new(JobStore::new());
        (create_digest_feature(Arc::new(|| json!({})), store.clone(), Arc::new(io)), store)
    }

    async fn settled(store: &JobStore, id: &str) -> Value {
        for _ in 0..200 {
            let j = store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the digest never finished");
    }

    #[tokio::test]
    async fn the_feature_is_a_read_only_polled_job_you_can_discard() {
        let (f, store) = feature(fake());
        let job = f.start(json!({}), FeatureContext::default()).await.unwrap();
        assert_eq!(job["featureId"], "digest");
        let done = settled(&store, job["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "awaiting-approval");
        assert!(done["result"]["summary"].as_str().unwrap().starts_with("Needs you:"));
        assert!(!done["data"]["digest"]["sections"].as_array().unwrap().is_empty());
        assert!(done.get("progress").is_none());
        let e = f.approve(done.clone(), FeatureContext::default()).await.unwrap_err();
        assert!(e.to_string().contains("read-only"));
        f.reject(done.clone(), FeatureContext::default()).await.unwrap();
        assert_eq!(store.get(job["id"].as_str().unwrap()).unwrap()["status"], "rejected");
        let again = f.reject(store.get(job["id"].as_str().unwrap()).unwrap(), FeatureContext::default()).await.unwrap_err();
        assert_eq!(again.to_string(), "Cannot reject job in status \"rejected\"");
        assert_eq!((f.id(), f.label()), ("digest", "Morning digest"));
    }

    #[tokio::test]
    async fn links_must_be_https_on_the_configured_host_others_are_dropped_from_the_digest() {
        let mut f = fake();
        f.prs = Box::new(|role| Ok(if role == "AUTHOR" { vec![pr(1, json!({ "url": "http://bb.example/x" })), pr(2, json!({ "url": "https://evil.example/y" }))] } else { vec![pr(9, json!({ "url": "javascript:alert(1)" }))] }));
        f.merge = Box::new(|_| Ok(json!({ "conflicted": true })));
        f.issues = Box::new(|| Ok(vec![json!({ "key": "PROJ-7", "summary": "x", "status": "Open", "priority": "High", "url": "https://evil.example/browse/PROJ-7" })]));
        let d = compute_digest(&f, &AuthContext::default(), NOW).await.unwrap();
        assert!(d.sections.iter().flat_map(|s| s.items.iter()).all(|i| i.url.is_none()), "nothing off-host or non-https survives");
        let ok = compute_digest(&fake(), &AuthContext::default(), NOW).await.unwrap();
        assert!(ok.sections.iter().flat_map(|s| s.items.iter()).any(|i| i.url.as_deref() == Some("https://jira.example/browse/PROJ-7")));
    }

    #[tokio::test]
    async fn error_text_in_notes_and_in_watcher_job_lines_is_redacted() {
        let mut f = fake();
        f.prs = Box::new(|_| Err(AtlassianError::Other("GET https://u:secret@bb/x failed: Bearer abc123".into())));
        f.jobs = Box::new(|| Ok(vec![json!({ "id": "a", "featureId": "analyze-issue", "status": "failed", "startedVia": "watcher", "createdAt": NOW - 1000.0, "error": "token=Bearer zzz999 rejected" })]));
        let d = compute_digest(&f, &AuthContext::default(), NOW).await.unwrap();
        let text = serde_json::to_string(&d).unwrap();
        assert!(!text.contains("secret") && !text.contains("abc123") && !text.contains("zzz999"), "{text}");
    }

    #[tokio::test]
    async fn per_pr_merge_and_build_failures_become_notes_only_10_prs_are_looked_at_a_failed_watcher_job_shows_as_failed() {
        let merges = Arc::new(Mutex::new(0));
        let m = merges.clone();
        let mut f = fake();
        f.prs = Box::new(|role| Ok(if role == "AUTHOR" { (1..=14).map(|i| pr(i, json!({}))).collect() } else { vec![] }));
        f.merge = Box::new(move |id| {
            *m.lock().unwrap() += 1;
            if id == 1 { Err(AtlassianError::Other("boom".into())) } else { Ok(json!({ "conflicted": false })) }
        });
        f.build = Box::new(|sha| if sha == "sha2000" || sha == "sha3000" { Err(AtlassianError::Other("boom".into())) } else { Ok(None) });
        f.jobs = Box::new(|| Ok(vec![json!({ "id": "a", "featureId": "resolve-conflict", "status": "failed", "startedVia": "watcher", "scopeKey": "bitbucket:CI/sample-app#1", "createdAt": NOW - 1000.0, "error": "no clone" })]));
        let d = compute_digest(&f, &AuthContext::default(), NOW).await.unwrap();
        assert_eq!(*merges.lock().unwrap(), 10);
        assert_eq!(texts(&d, "notes"), ["Couldn't check 1 of your PRs for conflicts.", "Couldn't read the builds of 2 of your PRs."]);
        let ready = d.sections.iter().find(|s| s.id == "ready").unwrap();
        assert_eq!(ready.items[0].tone, super::super::plan::Tone::Bad);
        assert!(ready.items[0].text.contains("failed"));
    }

    #[tokio::test]
    async fn a_digest_that_throws_inside_the_feature_fails_the_job_with_a_redacted_error() {
        let mut io = fake();
        io.jobs = Box::new(|| Err("disk Bearer abc123 broke".into()));
        let (f, store) = feature(io);
        let job = f.start(json!({}), FeatureContext::default()).await.unwrap();
        let done = settled(&store, job["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "failed");
        assert!(!done["error"].as_str().unwrap().contains("abc123"));
        assert!(f.get_digest(&AuthContext::default()).await.is_err());
    }
}
