//! What the extension asks about the pull request on the page: is it open, does
//! it conflict, which branches, is it from a fork (port of `core/pr-status.js`).
//! The extension can't ask the git host itself (GitHub's page cookies don't
//! authenticate its API, and Bitbucket's REST shape is its own business), so it
//! asks the companion, which reads the PR through the configured git provider
//! and answers in one host-neutral shape. Answers are cached briefly (the
//! extension re-checks the page every couple of seconds, which would otherwise
//! be a stream of API calls) and calls for the same PR that arrive together
//! share one lookup.

use crate::js_number::json_number;
use crate::prereqs::is_safe_repo_segment;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

pub const DEFAULT_TTL_MS: f64 = 20_000.0;
const ERROR_TTL_MS: f64 = 5_000.0;
const MAX_ENTRIES: usize = 200;

/// The slice of the git provider this needs.
pub trait PrGit: Send + Sync {
    /// The PR in the host-neutral shape (`state`, `fromBranch`, `toBranch`, `title`,
    /// `commentCount`, `openTaskCount`, `fromRepo`, `toRepo`), or `None`.
    fn get_pull_request(&self, auth: &Value, project: &str, repo: &str, pr_id: u64) -> impl Future<Output = Result<Option<Value>, String>> + Send;
    /// Whether the host can say if a PR conflicts.
    fn has_merge_status(&self) -> bool;
    /// `{conflicted, ...}`.
    fn get_merge_status(&self, auth: &Value, project: &str, repo: &str, pr_id: u64) -> impl Future<Output = Result<Value, String>> + Send;
    fn pr_url(&self, project: &str, repo: &str, pr_id: u64) -> String;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrStatusError {
    /// A request that isn't about a well-formed pull request (the route answers 400).
    Request(String),
    Lookup(String),
}

impl std::fmt::Display for PrStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrStatusError::Request(m) | PrStatusError::Lookup(m) => f.write_str(m),
        }
    }
}

fn same_name(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.to_lowercase() == b.to_lowercase())
}

/// True when the PR's source repository differs from its target, false when it
/// is the same, `None` when unknown.
pub fn is_fork(pr: &Value) -> Option<bool> {
    let (from, to) = (&pr["fromRepo"], &pr["toRepo"]);
    let has = |r: &Value| crate::js_text::js_truthy(r.get("projectKey")) && crate::js_text::js_truthy(r.get("slug"));
    if !has(from) || !has(to) {
        return None;
    }
    Some(!(same_name(from["projectKey"].as_str(), to["projectKey"].as_str()) && same_name(from["slug"].as_str(), to["slug"].as_str())))
}

fn num(v: &Value) -> Value {
    v.as_f64().filter(|n| n.is_finite()).map_or(Value::Null, json_number)
}

struct Entry {
    at: f64,
    ttl: Mutex<f64>,
    cell: OnceCell<Result<Value, String>>,
}

pub struct PrStatus<G: PrGit> {
    get_git: Box<dyn Fn() -> Arc<G> + Send + Sync>,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
    ttl_ms: f64,
    cache: Mutex<Vec<(String, Arc<Entry>)>>,
}

impl<G: PrGit> PrStatus<G> {
    pub fn new(get_git: impl Fn() -> Arc<G> + Send + Sync + 'static) -> Self {
        Self { get_git: Box::new(get_git), now: Box::new(crate::job_files::now_ms), ttl_ms: DEFAULT_TTL_MS, cache: Mutex::new(vec![]) }
    }

    pub fn with_clock(mut self, now: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    pub fn with_ttl_ms(mut self, ttl_ms: f64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    async fn lookup(git: &G, auth: &Value, project: &str, repo: &str, pr_id: u64) -> Result<Value, String> {
        let pr = git.get_pull_request(auth, project, repo, pr_id).await?.ok_or_else(|| format!("The git host didn't return pull request {project}/{repo}#{pr_id}."))?;
        let mut conflicted = Value::Null;
        // A merged or declined PR has nothing left to conflict, and the check costs a call.
        if pr["state"] == "OPEN" && git.has_merge_status() {
            // A failing check leaves it unknown.
            if let Ok(status) = git.get_merge_status(auth, project, repo, pr_id).await {
                if status["conflicted"].is_boolean() {
                    conflicted = status["conflicted"].clone();
                }
            }
        }
        let text = |k: &str| pr[k].as_str().map_or(Value::Null, |s| json!(s));
        Ok(json!({
            "state": text("state"),
            "conflicted": conflicted,
            "isFork": is_fork(&pr),
            "fromBranch": text("fromBranch"),
            "toBranch": text("toBranch"),
            "title": text("title"),
            "prUrl": git.pr_url(project, repo, pr_id),
            "commentCount": num(&pr["commentCount"]),
            "openTaskCount": num(&pr["openTaskCount"]),
        }))
    }

    pub async fn get(&self, auth: &Value, project: &str, repo: &str, pr_id: i64) -> Result<Value, PrStatusError> {
        if !is_safe_repo_segment(project) || !is_safe_repo_segment(repo) {
            return Err(PrStatusError::Request("project and repo must be plain names".into()));
        }
        if pr_id < 1 {
            return Err(PrStatusError::Request("prId must be a positive whole number".into()));
        }
        let key = format!("{}/{}#{pr_id}", project.to_lowercase(), repo.to_lowercase());
        let now = (self.now)();
        let entry = {
            let mut cache = self.cache.lock().unwrap();
            match cache.iter().find(|(k, _)| *k == key).filter(|(_, e)| now - e.at < *e.ttl.lock().unwrap()) {
                Some((_, e)) => e.clone(),
                None => {
                    let e = Arc::new(Entry { at: now, ttl: Mutex::new(self.ttl_ms), cell: OnceCell::new() });
                    cache.retain(|(k, _)| *k != key);
                    cache.push((key, e.clone()));
                    if cache.len() > MAX_ENTRIES {
                        cache.remove(0);
                    }
                    e
                }
            }
        };
        let git = (self.get_git)();
        let result = entry.cell.get_or_init(|| Self::lookup(&git, auth, project, repo, pr_id as u64)).await;
        // A failure is remembered only briefly, so a fixed token or a PR that appears later is picked up soon.
        if result.is_err() {
            *entry.ttl.lock().unwrap() = ERROR_TTL_MS;
        }
        result.clone().map_err(PrStatusError::Lookup)
    }

    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn pr() -> Value {
        json!({
            "id": 42, "state": "OPEN", "title": "Fix login", "fromBranch": "fix/login", "toBranch": "main",
            "fromRepo": { "projectKey": "Octo", "slug": "hello" }, "toRepo": { "projectKey": "octo", "slug": "Hello" }, "commentCount": 8,
        })
    }

    struct Fake {
        pr: Mutex<Option<Value>>,
        merge: bool,
        merge_answer: Mutex<Result<Value, String>>,
        pr_calls: AtomicUsize,
        merge_calls: AtomicUsize,
        fail: AtomicBool,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self { pr: Mutex::new(Some(pr())), merge: true, merge_answer: Mutex::new(Ok(json!({ "conflicted": true, "canMerge": false, "outcome": "DIRTY" }))), pr_calls: 0.into(), merge_calls: 0.into(), fail: false.into() })
        }
    }

    impl PrGit for Fake {
        async fn get_pull_request(&self, _a: &Value, _p: &str, _r: &str, _n: u64) -> Result<Option<Value>, String> {
            self.pr_calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            if self.fail.load(Ordering::SeqCst) {
                return Err("no token".into());
            }
            Ok(self.pr.lock().unwrap().clone())
        }
        fn has_merge_status(&self) -> bool {
            self.merge
        }
        async fn get_merge_status(&self, _a: &Value, _p: &str, _r: &str, _n: u64) -> Result<Value, String> {
            self.merge_calls.fetch_add(1, Ordering::SeqCst);
            self.merge_answer.lock().unwrap().clone()
        }
        fn pr_url(&self, p: &str, r: &str, n: u64) -> String {
            format!("https://git.example.com/{p}/{r}/pull/{n}")
        }
    }

    fn status(f: &Arc<Fake>) -> PrStatus<Fake> {
        let f = f.clone();
        PrStatus::new(move || f.clone())
    }

    #[tokio::test]
    async fn answers_in_one_host_neutral_shape() {
        let f = Fake::new();
        let out = status(&f).get(&json!({}), "octo", "hello", 42).await.unwrap();
        assert_eq!(
            out,
            json!({ "state": "OPEN", "conflicted": true, "isFork": false, "fromBranch": "fix/login", "toBranch": "main", "title": "Fix login",
                    "prUrl": "https://git.example.com/octo/hello/pull/42", "commentCount": 8, "openTaskCount": null })
        );
        assert_eq!(
            out.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            ["state", "conflicted", "isFork", "fromBranch", "toBranch", "title", "prUrl", "commentCount", "openTaskCount"]
        );
    }

    #[tokio::test]
    async fn a_pr_that_isnt_open_is_not_checked_for_conflicts() {
        let f = Fake::new();
        *f.pr.lock().unwrap() = Some(json!({ "state": "MERGED" }));
        let out = status(&f).get(&json!({}), "o", "r", 1).await.unwrap();
        assert_eq!((out["state"].as_str(), out["conflicted"].clone()), (Some("MERGED"), Value::Null));
        assert_eq!(f.merge_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_host_without_merge_status_or_a_merge_check_that_fails_leaves_conflicted_unknown() {
        let none = Arc::new(Fake { merge: false, ..Arc::try_unwrap(Fake::new()).ok().unwrap() });
        assert_eq!(status(&none).get(&json!({}), "o", "r", 1).await.unwrap()["conflicted"], Value::Null);
        let broken = Fake::new();
        *broken.merge_answer.lock().unwrap() = Err("boom".into());
        assert_eq!(status(&broken).get(&json!({}), "o", "r", 1).await.unwrap()["conflicted"], Value::Null);
        let unknown = Fake::new();
        *unknown.merge_answer.lock().unwrap() = Ok(json!({ "conflicted": null }));
        assert_eq!(status(&unknown).get(&json!({}), "o", "r", 1).await.unwrap()["conflicted"], Value::Null);
    }

    #[test]
    fn is_fork_same_repo_case_insensitive_is_not_a_fork_a_different_one_is_a_missing_one_is_unknown() {
        assert_eq!(is_fork(&pr()), Some(false));
        assert_eq!(is_fork(&json!({ "fromRepo": { "projectKey": "me", "slug": "hello" }, "toRepo": { "projectKey": "octo", "slug": "hello" } })), Some(true));
        assert_eq!(is_fork(&json!({ "fromRepo": { "projectKey": null, "slug": null }, "toRepo": { "projectKey": "octo", "slug": "hello" } })), None);
        assert_eq!(is_fork(&json!({})), None);
        assert_eq!(is_fork(&Value::Null), None);
    }

    #[tokio::test]
    async fn the_same_pr_is_looked_up_once_within_the_cache_window_and_again_after_it() {
        let f = Fake::new();
        let t = Arc::new(Mutex::new(1000.0));
        let t2 = t.clone();
        let s = status(&f).with_clock(move || *t2.lock().unwrap()).with_ttl_ms(20_000.0);
        s.get(&json!({}), "octo", "hello", 42).await.unwrap();
        s.get(&json!({}), "OCTO", "Hello", 42).await.unwrap();
        *t.lock().unwrap() += 19_000.0;
        s.get(&json!({}), "octo", "hello", 42).await.unwrap();
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 1);
        *t.lock().unwrap() += 2_000.0;
        s.get(&json!({}), "octo", "hello", 42).await.unwrap();
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 2);
        s.get(&json!({}), "octo", "hello", 43).await.unwrap();
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 3, "another PR is another lookup");
        s.clear();
        s.get(&json!({}), "octo", "hello", 43).await.unwrap();
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn calls_that_arrive_together_share_one_lookup() {
        let f = Fake::new();
        let s = status(&f);
        let a = json!({});
        let (x, y, z) = tokio::join!(s.get(&a, "o", "r", 1), s.get(&a, "o", "r", 1), s.get(&a, "o", "r", 1));
        assert!(x.is_ok() && y.is_ok() && z.is_ok());
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failure_is_remembered_for_a_few_seconds_only() {
        let f = Fake::new();
        f.fail.store(true, Ordering::SeqCst);
        let t = Arc::new(Mutex::new(0.0));
        let t2 = t.clone();
        let s = status(&f).with_clock(move || *t2.lock().unwrap());
        assert_eq!(s.get(&json!({}), "o", "r", 1).await.unwrap_err(), PrStatusError::Lookup("no token".into()));
        assert!(s.get(&json!({}), "o", "r", 1).await.is_err());
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 1, "within the error window it is not asked again");
        f.fail.store(false, Ordering::SeqCst);
        *t.lock().unwrap() += 6_000.0;
        assert_eq!(s.get(&json!({}), "o", "r", 1).await.unwrap()["state"], "OPEN");
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_pr_the_host_doesnt_return_is_an_error() {
        let f = Fake::new();
        *f.pr.lock().unwrap() = None;
        let e = status(&f).get(&json!({}), "o", "r", 5).await.unwrap_err();
        assert!(e.to_string().contains("didn't return pull request o/r#5"), "{e}");
    }

    #[tokio::test]
    async fn anything_that_isnt_a_plain_name_and_a_positive_number_is_refused_before_a_lookup() {
        let f = Fake::new();
        let s = status(&f);
        for (p, r, n) in [("o", "../x", 1), ("o/x", "r", 1), ("", "r", 1), ("o", "r", 0), ("o", "r", -1)] {
            assert!(matches!(s.get(&json!({}), p, r, n).await, Err(PrStatusError::Request(_))), "{p} {r} {n}");
        }
        assert_eq!(f.pr_calls.load(Ordering::SeqCst), 0);
    }
}
