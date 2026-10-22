//! Moves the ticket to its review status once a pull request is open (port of
//! `features/ticket-to-pr/review-mover.ts`). After Start fix the fix is committed, pushed and opened
//! as a pull request from the Claude Code terminal, where Bitbucket fills in the default reviewers
//! and Jira links the PR by the ticket key. The one step left is moving the ticket; a poll does it on
//! its own (Settings can turn that off) and the panel's "Move ticket to In Review" does it on
//! request. The job then ends as "approved" with the PR recorded, which is what the history counts as
//! Start fix to PR.

use super::plan::run_review_move;
use super::plan::TransitionDeps;
use crate::atlassian::AuthContext;
use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::config::ConfigFn;
use crate::features::ticket_workspace::plan::pr_url_allowed;
use crate::jira_workflow::Transition;
use crate::jobs::JobStore;
use crate::prereqs::GitRemoteRules;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_secs(3 * 60);

/// What the mover reads and writes outside this process; tests inject it.
#[async_trait]
pub trait MoverIo: Send + Sync {
    async fn find_open_pr(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Option<Value>, String>;
    async fn list_transitions(&self, auth: &AuthContext, issue_key: &str) -> Result<Vec<Transition>, String>;
    async fn transition(&self, auth: &AuthContext, issue_key: &str, id: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    /// "moved" | "no-transition" | "failed" | "waiting" (no pull request yet, polling only) | "already".
    pub status: &'static str,
    pub detail: String,
    pub pr_id: Option<i64>,
}

fn outcome(status: &'static str, detail: String) -> MoveOutcome {
    MoveOutcome { status, detail, pr_id: None }
}

struct Tracked {
    issue_key: String,
    project: String,
    repo: String,
    branch: String,
}

/// Moving the ticket, for the shared plan step.
struct Moving<'a> {
    io: &'a dyn MoverIo,
    auth: &'a AuthContext,
    issue_key: &'a str,
}

#[async_trait]
impl TransitionDeps for Moving<'_> {
    async fn list_transitions(&self) -> Result<Vec<Transition>, String> {
        self.io.list_transitions(self.auth, self.issue_key).await
    }
    async fn transition(&self, id: &str) -> Result<(), String> {
        self.io.transition(self.auth, self.issue_key, id).await
    }
}

pub struct ReviewMover {
    store: Arc<JobStore>,
    config: ConfigFn,
    io: Arc<dyn MoverIo>,
    /// The git host's configured address; an error means no PR address is kept.
    git_base_url: Arc<dyn Fn() -> Result<String, String> + Send + Sync>,
    rules: GitRemoteRules,
    in_flight: Mutex<HashSet<String>>,
}

pub fn create_review_mover(store: Arc<JobStore>, config: ConfigFn, io: Arc<dyn MoverIo>, git_base_url: Arc<dyn Fn() -> Result<String, String> + Send + Sync>, rules: GitRemoteRules) -> Arc<ReviewMover> {
    Arc::new(ReviewMover { store, config, io, git_base_url, rules, in_flight: Mutex::new(HashSet::new()) })
}

fn trackable(job: &Value) -> Option<Tracked> {
    if job["featureId"] != "ticket-to-pr" || job["status"] != "awaiting-approval" {
        return None;
    }
    let d = &job["data"];
    let (issue_key, repo_key, branch) = (d["issueKey"].as_str().filter(|s| !s.is_empty())?, d["repoKey"].as_str().filter(|s| !s.is_empty())?, d["branch"].as_str().filter(|s| !s.is_empty() && is_safe_branch_name(s))?);
    let mut parts = repo_key.split('/');
    let (project, repo) = (parts.next().filter(|s| !s.is_empty())?, parts.next().filter(|s| !s.is_empty())?);
    if parts.next().is_some() {
        return None;
    }
    Some(Tracked { issue_key: issue_key.into(), project: project.into(), repo: repo.into(), branch: branch.into() })
}

impl ReviewMover {
    /// A PR address is only kept when it's a PR page on the configured git host.
    fn safe_pr(&self, pr: &Value) -> Value {
        let ok = pr["url"].as_str().is_some_and(|u| (self.git_base_url)().is_ok_and(|base| pr_url_allowed(u, &base, self.rules)));
        json!({ "id": pr["id"], "url": if ok { pr["url"].clone() } else { Value::Null }, "title": pr.get("title").cloned().unwrap_or(Value::Null) })
    }

    fn patch(&self, id: &str, v: Value) {
        let _ = self.store.update(id, v.as_object().cloned().unwrap_or_default());
    }

    /// One job: look for the open pull request (when `need_pr`, nothing is done without one), move
    /// the ticket unless that's already settled, and end the job once there is a PR. A failure is
    /// recorded on the job and tried again on the next poll.
    async fn check(&self, job: &Value, auth: &AuthContext, need_pr: bool) -> MoveOutcome {
        let Some(t) = trackable(job) else { return outcome("already", "This job isn't waiting for a pull request.".into()) };
        let id = job["id"].as_str().unwrap_or("").to_string();
        if !self.in_flight.lock().unwrap().insert(id.clone()) {
            return outcome("already", "Already checking this job.".into());
        }
        let result = self.check_inner(job, auth, need_pr, &t, &id).await;
        self.in_flight.lock().unwrap().remove(&id);
        result
    }

    async fn check_inner(&self, job: &Value, auth: &AuthContext, need_pr: bool, t: &Tracked, id: &str) -> MoveOutcome {
        let pr = match self.io.find_open_pr(auth, &t.project, &t.repo, &t.branch).await {
            Ok(pr) => pr,
            Err(e) if need_pr => return outcome("failed", format!("Couldn't look for the pull request: {e}")),
            Err(_) => None,
        };
        if need_pr && pr.is_none() {
            return outcome("waiting", format!("No open pull request from {} yet.", t.branch));
        }

        let previous = &job["data"]["reviewMove"];
        let mut out = if previous["moved"] == json!(true) {
            outcome("already", previous["detail"].as_str().unwrap_or("").to_string())
        } else {
            let config = (self.config)();
            let name = config["ticketToPr"]["reviewTransitionName"].as_str().filter(|n| !n.is_empty());
            let result = run_review_move(&t.issue_key, name, &Moving { io: self.io.as_ref(), auth, issue_key: &t.issue_key }).await;
            // "moved" and "no-transition" are settled; a failure is tried again.
            let _ = self.store.patch_data(
                id,
                Map::from_iter([("reviewMove".to_string(), json!({ "moved": result.status != "failed", "status": result.status, "detail": result.detail, "at": crate::job_files::now_ms() as i64 }))]),
            );
            outcome(result.status, result.detail)
        };

        let settled = matches!(out.status, "moved" | "no-transition" | "already");
        if let (Some(pr), true) = (&pr, settled) {
            out.pr_id = pr["id"].as_i64();
            // Data first, then the status: the history records the job as its status changes.
            let _ = self.store.patch_data(id, Map::from_iter([("pr".to_string(), self.safe_pr(pr))]));
            self.patch(id, json!({ "status": "approved", "progress": null, "result": { "summary": format!("Pull request #{} is open. {}", crate::js_text::js_to_string(&pr["id"]), out.detail), "files": [] } }));
        }
        out
    }

    /// One poll over every tracked job; how many ended.
    pub async fn tick(&self) -> usize {
        if (self.config)()["ticketToPr"]["autoMoveToReview"] == json!(false) {
            return 0;
        }
        let mut ended = 0;
        for job in self.store.list() {
            if trackable(&job).is_none() {
                continue;
            }
            let out = self.check(&job, &AuthContext::default(), true).await;
            if out.pr_id.is_some() {
                ended += 1;
            }
            let key = job["data"]["issueKey"].as_str().unwrap_or("");
            match out.status {
                "failed" => eprintln!("[ticket-to-pr] {key}: {}", out.detail),
                "moved" => println!("[ticket-to-pr] {key}: {}", out.detail),
                _ => {}
            }
        }
        ended
    }

    /// The panel's button: moves the ticket now, PR or not.
    pub async fn move_now(&self, job: &Value, auth: &AuthContext) -> MoveOutcome {
        self.check(job, auth, false).await
    }

    /// Starts the poll; returns how to stop it. Needs a running tokio runtime.
    pub fn start(self: &Arc<Self>, interval: Duration) -> Box<dyn FnOnce() + Send> {
        let this = self.clone();
        let handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.tick().await; // the first tick fires at once; the poll starts one interval in
            loop {
                timer.tick().await;
                this.tick().await;
            }
        });
        Box::new(move || handle.abort())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://bb.example.com";

    fn pr() -> Value {
        json!({ "id": 12, "title": "PROJ-7: fix", "state": "OPEN", "url": format!("{BASE}/projects/ACME/repos/sample-app/pull-requests/12") })
    }

    fn t_review() -> Transition {
        Transition { id: "31".into(), name: Some("Start review".into()), to: Some("In Review".into()) }
    }

    struct FakeIo {
        pr: Option<Value>,
        transitions: Vec<Transition>,
        fail_transition: bool,
        find: Mutex<usize>,
        moved: Mutex<Vec<(String, String)>>,
    }

    fn fake(pr: Option<Value>, transitions: Vec<Transition>, fail_transition: bool) -> Arc<FakeIo> {
        Arc::new(FakeIo { pr, transitions, fail_transition, find: Mutex::new(0), moved: Mutex::new(vec![]) })
    }

    #[async_trait]
    impl MoverIo for FakeIo {
        async fn find_open_pr(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> Result<Option<Value>, String> {
            *self.find.lock().unwrap() += 1;
            Ok(self.pr.clone())
        }
        async fn list_transitions(&self, _: &AuthContext, _: &str) -> Result<Vec<Transition>, String> {
            Ok(self.transitions.clone())
        }
        async fn transition(&self, _: &AuthContext, key: &str, id: &str) -> Result<(), String> {
            if self.fail_transition {
                return Err("Jira said no".into());
            }
            self.moved.lock().unwrap().push((key.into(), id.into()));
            Ok(())
        }
    }

    fn mover(store: &Arc<JobStore>, config: Value, io: &Arc<FakeIo>) -> Arc<ReviewMover> {
        create_review_mover(store.clone(), Arc::new(move || config.clone()), io.clone(), Arc::new(|| Ok(BASE.to_string())), GitRemoteRules::BitbucketDc)
    }

    fn base_config() -> Value {
        json!({ "repos": {}, "ticketToPr": {}, "bitbucket": { "baseUrl": BASE } })
    }

    fn tracked(store: &JobStore, extra: Value) -> String {
        let job = store.create("ticket-to-pr", Map::from_iter([("issueKey".to_string(), json!("PROJ-7")), ("adopt".to_string(), json!(false))]));
        let id = job["id"].as_str().unwrap().to_string();
        let mut data = json!({ "repoKey": "ACME/sample-app", "branch": "bugfix/PROJ-7-login" });
        for (k, v) in extra.as_object().unwrap() {
            data[k] = v.clone();
        }
        store.patch_data(&id, data.as_object().unwrap().clone()).unwrap();
        store.update(&id, Map::from_iter([("status".to_string(), json!("awaiting-approval"))])).unwrap();
        id
    }

    #[tokio::test]
    async fn a_tick_with_an_open_pr_moves_the_ticket_records_the_pr_and_ends_the_job_as_approved() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let io = fake(Some(pr()), vec![t_review()], false);
        assert!(mover(&store, base_config(), &io).tick().await >= 1);
        assert_eq!(*io.moved.lock().unwrap(), [("PROJ-7".to_string(), "31".to_string())]);
        let after = store.get(&id).unwrap();
        assert_eq!(after["status"], "approved");
        assert_eq!(after["data"]["reviewMove"]["moved"], true);
        assert_eq!(after["data"]["pr"], pr().as_object().map(|p| json!({ "id": p["id"], "url": p["url"], "title": p["title"] })).unwrap());
        assert!(after["result"]["summary"].as_str().unwrap().contains("Pull request #12 is open. Moved PROJ-7 to In Review."));
    }

    #[tokio::test]
    async fn a_tick_with_no_open_pr_changes_nothing_and_moves_nothing() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let io = fake(None, vec![t_review()], false);
        mover(&store, base_config(), &io).tick().await;
        assert!(io.moved.lock().unwrap().is_empty());
        let job = store.get(&id).unwrap();
        assert_eq!(job["status"], "awaiting-approval");
        assert!(job["data"].get("reviewMove").is_none());
    }

    #[tokio::test]
    async fn auto_move_to_review_false_stops_the_poll_but_not_the_button() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let io = fake(Some(pr()), vec![t_review()], false);
        let m = mover(&store, json!({ "ticketToPr": { "autoMoveToReview": false }, "bitbucket": { "baseUrl": BASE } }), &io);
        assert_eq!(m.tick().await, 0);
        assert_eq!(*io.find.lock().unwrap(), 0);
        assert_eq!(store.get(&id).unwrap()["status"], "awaiting-approval");
        let out = m.move_now(&store.get(&id).unwrap(), &AuthContext::default()).await;
        assert_eq!(out.status, "moved");
        assert_eq!(store.get(&id).unwrap()["status"], "approved"); // a PR exists too, so the job ends
    }

    #[tokio::test]
    async fn the_button_moves_the_ticket_before_a_pr_exists_and_the_poll_later_ends_the_job_without_moving_again() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let io = fake(None, vec![t_review()], false);
        let out = mover(&store, base_config(), &io).move_now(&store.get(&id).unwrap(), &AuthContext::default()).await;
        assert_eq!(out.status, "moved");
        assert_eq!(*io.moved.lock().unwrap(), [("PROJ-7".to_string(), "31".to_string())]);
        assert_eq!(store.get(&id).unwrap()["status"], "awaiting-approval");
        assert_eq!(store.get(&id).unwrap()["data"]["reviewMove"]["moved"], true);
        let later = fake(Some(pr()), vec![t_review()], false);
        mover(&store, base_config(), &later).tick().await;
        assert!(later.moved.lock().unwrap().is_empty());
        assert_eq!(store.get(&id).unwrap()["status"], "approved");
    }

    #[tokio::test]
    async fn a_ticket_with_no_matching_transition_is_settled_not_retried() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let io = fake(Some(pr()), vec![Transition { id: "9".into(), name: Some("Close".into()), to: Some("Done".into()) }], false);
        mover(&store, base_config(), &io).tick().await;
        assert!(io.moved.lock().unwrap().is_empty());
        assert_eq!(store.get(&id).unwrap()["data"]["reviewMove"]["status"], "no-transition");
        assert_eq!(store.get(&id).unwrap()["status"], "approved");
    }

    #[tokio::test]
    async fn a_failed_move_is_recorded_leaves_the_job_waiting_and_is_tried_again() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let bad = fake(None, vec![t_review()], true);
        let out = mover(&store, base_config(), &bad).move_now(&store.get(&id).unwrap(), &AuthContext::default()).await;
        assert_eq!(out.status, "failed");
        assert_eq!(store.get(&id).unwrap()["status"], "awaiting-approval");
        assert_eq!(store.get(&id).unwrap()["data"]["reviewMove"]["moved"], false);
        let good = fake(Some(pr()), vec![t_review()], false);
        mover(&store, base_config(), &good).tick().await;
        assert_eq!(*good.moved.lock().unwrap(), [("PROJ-7".to_string(), "31".to_string())]);
        assert_eq!(store.get(&id).unwrap()["status"], "approved");
    }

    #[tokio::test]
    async fn jobs_that_are_not_waiting_for_a_pr_or_have_an_unsafe_branch_are_ignored() {
        let store = Arc::new(JobStore::new());
        let other = store.create("analyze-issue", Map::from_iter([("issueKey".to_string(), json!("PROJ-8"))]));
        store.update(other["id"].as_str().unwrap(), Map::from_iter([("status".to_string(), json!("awaiting-approval"))])).unwrap();
        let unsafe_job = tracked(&store, json!({ "branch": "--upload-pack=evil" }));
        let no_branch = tracked(&store, json!({ "branch": null }));
        let io = fake(Some(pr()), vec![t_review()], false);
        mover(&store, base_config(), &io).tick().await;
        for id in [other["id"].as_str().unwrap(), &unsafe_job, &no_branch] {
            assert_eq!(store.get(id).unwrap()["status"], "awaiting-approval");
        }
        assert!(io.moved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pr_address_off_the_configured_host_is_not_kept_as_a_link() {
        let store = Arc::new(JobStore::new());
        let id = tracked(&store, json!({}));
        let mut evil = pr();
        evil["url"] = json!("https://evil.example/projects/ACME/repos/sample-app/pull-requests/12");
        let io = fake(Some(evil), vec![t_review()], false);
        mover(&store, base_config(), &io).tick().await;
        assert_eq!(store.get(&id).unwrap()["data"]["pr"]["url"], Value::Null);
    }
}
