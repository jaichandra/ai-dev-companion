//! The "Which repository?" question a running job can put to the user
//! (`job.data.pendingChoice`, rendered by the extension's repo-choice buttons)
//! and the answer that settles it (port of `core/repo-choice.ts`). Shared by
//! analyze-issue and ticket-to-pr.

use crate::jobs::{FeatureError, JobStore};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// One repository offered in the question.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoOption {
    pub repo_key: String,
    pub reason: String,
}

/// `prompt` and `no_repo_label` override the panel's default wording (written for Analyze ticket).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoQuestion {
    pub suggested: Option<String>,
    pub options: Vec<RepoOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_repo_label: Option<String>,
    /// The "no" answer only cancels the job, which the panel's own Cancel button does too: the
    /// panel leaves this one out for a cancellable feature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub null_is_cancel: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RepoChoiceError {
    /// The waiting job was aborted.
    Cancelled,
    /// No answer within the time limit; carries the full message.
    TimedOut(String),
    /// The question couldn't be published (the job is gone).
    Job(String),
}

impl std::fmt::Display for RepoChoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::TimedOut(m) | Self::Job(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for RepoChoiceError {}

struct Pending {
    offered: HashSet<String>,
    answer: oneshot::Sender<Option<String>>,
}

pub const REPO_CHOICE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Jobs waiting for an answer: the repos offered (held here, never taken from the request) and how
/// to deliver it.
pub struct RepoChoices {
    store: Arc<JobStore>,
    pending: Mutex<HashMap<String, Pending>>,
    timeout: Duration,
}

impl RepoChoices {
    pub fn new(store: Arc<JobStore>) -> Self {
        Self { store, pending: Mutex::new(HashMap::new()), timeout: REPO_CHOICE_TIMEOUT }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn take(&self, job_id: &str) {
        self.pending.lock().unwrap().remove(job_id);
        // The job may be gone; nothing to clear then.
        let _ = self.store.set_data(job_id, "pendingChoice", Value::Null);
    }

    /// Publishes the question on the job and resolves with the repo the user picked, or `None` for
    /// "no repo". Cancelling the token or no answer within the time limit ends the wait.
    /// `retry_hint` finishes the timeout message.
    pub async fn ask(&self, job_id: &str, cancel: Option<&CancellationToken>, question: &RepoQuestion, retry_hint: &str) -> Result<Option<String>, RepoChoiceError> {
        let (tx, rx) = oneshot::channel();
        let offered = question.options.iter().map(|o| o.repo_key.clone()).collect();
        self.pending.lock().unwrap().insert(job_id.to_string(), Pending { offered, answer: tx });
        let published = serde_json::to_value(question).expect("a question serializes");
        if let Err(e) = self.store.set_data(job_id, "pendingChoice", published) {
            self.pending.lock().unwrap().remove(job_id);
            return Err(RepoChoiceError::Job(e));
        }
        let never = CancellationToken::new();
        let cancelled = cancel.unwrap_or(&never).cancelled();
        let outcome = tokio::select! {
            answer = rx => answer.map_err(|_| RepoChoiceError::Cancelled),
            () = cancelled => Err(RepoChoiceError::Cancelled),
            () = tokio::time::sleep(self.timeout) => Err(RepoChoiceError::TimedOut(format!("No answer to \u{201c}Which repository?\u{201d} within 30 minutes \u{2014} {retry_hint}"))),
        };
        self.take(job_id);
        outcome
    }

    /// The feature action behind the buttons. Body `{ repoKey }`: one of the repos offered, or null.
    pub fn confirm(&self, job: &Value, body: Option<&Value>) -> Result<Value, FeatureError> {
        let id = job["id"].as_str().unwrap_or_default();
        let mut pending = self.pending.lock().unwrap();
        let Some(p) = pending.get(id) else { return Err(FeatureError::JobStatus("Nothing is waiting for a repository answer.".into())) };
        let repo_key = body.and_then(|b| b.get("repoKey"));
        let answer = match repo_key {
            Some(Value::Null) => None,
            Some(Value::String(k)) if p.offered.contains(k) => Some(k.clone()),
            _ => return Err(FeatureError::JobStatus("That isn't one of the repositories offered.".into())),
        };
        let p = pending.remove(id).expect("checked above");
        drop(pending);
        let _ = p.answer.send(answer);
        // The waiting `ask` clears the question; give it the job as it is now.
        Ok(self.store.get(id).unwrap_or(json!(null)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn setup() -> (Arc<JobStore>, Arc<RepoChoices>, String) {
        let store = Arc::new(JobStore::new());
        let id = store.create("f", Map::new())["id"].as_str().unwrap().to_string();
        (store.clone(), Arc::new(RepoChoices::new(store)), id)
    }

    fn question() -> RepoQuestion {
        RepoQuestion { suggested: Some("a/b".into()), options: vec![RepoOption { repo_key: "a/b".into(), reason: "named".into() }, RepoOption { repo_key: "c/d".into(), reason: "guess".into() }], prompt: None, no_repo_label: None, null_is_cancel: None }
    }

    async fn until_published(store: &JobStore, id: &str) {
        for _ in 0..200 {
            if !store.get(id).unwrap()["data"]["pendingChoice"].is_null() && store.get(id).unwrap()["data"].get("pendingChoice").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the question was never published");
    }

    #[tokio::test]
    async fn the_question_is_published_then_an_offered_answer_settles_it_and_clears_the_question() {
        let (store, choices, id) = setup();
        let c = choices.clone();
        let i = id.clone();
        let waiting = tokio::spawn(async move { c.ask(&i, None, &question(), "retry").await });
        until_published(&store, &id).await;
        let published = &store.get(&id).unwrap()["data"]["pendingChoice"];
        assert_eq!((published["suggested"].clone(), published["options"][1]["repoKey"].clone()), (json!("a/b"), json!("c/d")));
        assert!(published.get("prompt").is_none(), "unset wording is left out, as undefined is in JSON");
        choices.confirm(&store.get(&id).unwrap(), Some(&json!({ "repoKey": "c/d" }))).unwrap();
        assert_eq!(waiting.await.unwrap().unwrap().as_deref(), Some("c/d"));
        assert_eq!(store.get(&id).unwrap()["data"]["pendingChoice"], Value::Null);
        assert!(matches!(choices.confirm(&store.get(&id).unwrap(), Some(&json!({ "repoKey": "c/d" }))), Err(FeatureError::JobStatus(m)) if m == "Nothing is waiting for a repository answer."));
    }

    #[tokio::test]
    async fn null_means_no_repo_and_a_repo_that_wasnt_offered_is_refused_without_ending_the_wait() {
        let (store, choices, id) = setup();
        let (c, i) = (choices.clone(), id.clone());
        let waiting = tokio::spawn(async move { c.ask(&i, None, &question(), "retry").await });
        until_published(&store, &id).await;
        let job = store.get(&id).unwrap();
        for bad in [json!({ "repoKey": "evil/x" }), json!({ "repoKey": 5 }), json!({}), json!(null)] {
            let r = choices.confirm(&job, Some(&bad));
            assert!(matches!(r, Err(FeatureError::JobStatus(m)) if m == "That isn't one of the repositories offered."), "{bad}");
        }
        assert!(choices.confirm(&job, None).is_err());
        choices.confirm(&job, Some(&json!({ "repoKey": null }))).unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), None);
    }

    #[tokio::test]
    async fn cancelling_ends_the_wait_and_clears_the_question() {
        let (store, choices, id) = setup();
        let cancel = CancellationToken::new();
        let (c, i, t) = (choices.clone(), id.clone(), cancel.clone());
        let waiting = tokio::spawn(async move { c.ask(&i, Some(&t), &question(), "retry").await });
        until_published(&store, &id).await;
        cancel.cancel();
        assert_eq!(waiting.await.unwrap(), Err(RepoChoiceError::Cancelled));
        assert_eq!(store.get(&id).unwrap()["data"]["pendingChoice"], Value::Null);
        assert!(choices.confirm(&store.get(&id).unwrap(), Some(&json!({ "repoKey": null }))).is_err());
    }

    #[tokio::test]
    async fn no_answer_in_time_fails_with_the_hint_and_an_unknown_job_cannot_be_asked() {
        let (store, _, id) = setup();
        let choices = RepoChoices::new(store.clone()).with_timeout(Duration::from_millis(20));
        let e = choices.ask(&id, None, &question(), "start the job again.").await.unwrap_err();
        assert_eq!(e.to_string(), "No answer to \u{201c}Which repository?\u{201d} within 30 minutes \u{2014} start the job again.");
        assert_eq!(store.get(&id).unwrap()["data"]["pendingChoice"], Value::Null);
        let e = choices.ask("nope", None, &question(), "x").await.unwrap_err();
        assert_eq!(e, RepoChoiceError::Job("Unknown job id: nope".into()));
    }
}
