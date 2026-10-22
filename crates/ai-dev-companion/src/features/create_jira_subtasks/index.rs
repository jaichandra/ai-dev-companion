//! Create Subtasks (port of `features/create-jira-subtasks/index.ts`). No separate
//! approve step: the extension's form already collected exactly what to create, so
//! `start` does the whole thing (re-verify the ticket, create every row) and only
//! returns once the job has reached a terminal status.

use super::plan::{assert_subtask_rows, parse_issue_key, SubtaskRow};
use crate::atlassian::AuthContext;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::providers::{IssueCapability, IssueTracker, NewSubtask};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// What this feature needs from the issue tracker.
#[async_trait]
pub trait SubtaskApi: Send + Sync {
    fn supports_subtasks(&self) -> bool;
    fn parent_types(&self) -> Vec<String>;
    async fn get_issue_raw(&self, auth: &AuthContext, key: &str, fields: &str) -> Result<Value, String>;
    /// The created issue's key, when the tracker said.
    async fn create_subtask(&self, auth: &AuthContext, s: &NewSubtask) -> Result<Option<String>, String>;
}

/// The real tracker.
pub struct TrackerSubtasks(pub Arc<dyn IssueTracker>);

#[async_trait]
impl SubtaskApi for TrackerSubtasks {
    fn supports_subtasks(&self) -> bool {
        self.0.has(IssueCapability::Subtasks)
    }
    fn parent_types(&self) -> Vec<String> {
        self.0.subtask_parent_types().to_vec()
    }
    async fn get_issue_raw(&self, auth: &AuthContext, key: &str, fields: &str) -> Result<Value, String> {
        self.0.get_issue_raw(auth, key, fields).await.map_err(|e| e.to_string())
    }
    async fn create_subtask(&self, auth: &AuthContext, s: &NewSubtask) -> Result<Option<String>, String> {
        let created = self.0.create_subtask(auth, s).await.map_err(|e| e.to_string())?;
        Ok(created["key"].as_str().filter(|k| !k.is_empty()).map(String::from))
    }
}

pub struct CreateJiraSubtasks {
    store: Arc<JobStore>,
    api: Arc<dyn SubtaskApi>,
}

pub fn create_jira_subtasks_feature(store: Arc<JobStore>, api: Arc<dyn SubtaskApi>) -> Arc<CreateJiraSubtasks> {
    Arc::new(CreateJiraSubtasks { store, api })
}

fn patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.update(id, v.as_object().cloned().unwrap_or_default());
}

impl CreateJiraSubtasks {
    async fn run_job(&self, job_id: &str, issue_key: &str, project_from_key: &str, subtasks: &[SubtaskRow], auth: &AuthContext) {
        let fail = |error: String| patch(&self.store, job_id, json!({ "status": "failed", "error": error }));
        if !self.api.supports_subtasks() {
            return fail("This issue tracker can't create subtasks.".into());
        }
        // Re-verified server-side (not just trusted from the extension's client-side condition,
        // which can be stale by the time this runs): the ticket must exist and be a parent type
        // the tracker allows. This same call also proves auth works before anything is created.
        let issue = match self.api.get_issue_raw(auth, issue_key, "issuetype,project").await {
            Ok(i) => i,
            Err(e) => return fail(e),
        };
        let type_name = issue["fields"]["issuetype"]["name"].as_str().filter(|t| !t.is_empty());
        let parents = self.api.parent_types();
        let Some(type_name) = type_name.filter(|t| parents.iter().any(|p| p == t)) else {
            return fail(format!("{issue_key} is a {}, not a {} \u{2014} subtasks can only be added to those.", type_name.unwrap_or("ticket"), parents.join(" or ")));
        };
        let _ = type_name;
        let project_key = issue["fields"]["project"]["key"].as_str().filter(|k| !k.is_empty()).unwrap_or(project_from_key).to_string();

        // The tracker has no "create N issues atomically" endpoint (each POST is its own commit),
        // so `created` is named in the error on a partial failure, rather than lost, so a retry
        // isn't blind about what already exists.
        let mut created: Vec<String> = vec![];
        for s in subtasks {
            match self.api.create_subtask(auth, &NewSubtask { project_key: project_key.clone(), parent_key: issue_key.into(), summary: s.summary.clone(), assignee: s.assignee.clone() }).await {
                Ok(Some(key)) => created.push(key),
                Ok(None) => {}
                Err(e) => {
                    let partial = if created.is_empty() { String::new() } else { format!(" The following were already created before this failure: {}.", created.join(", ")) };
                    patch(&self.store, job_id, json!({ "status": "failed", "error": format!("{e}{partial}"), "data": { "issueKey": issue_key, "projectKey": project_key, "createdKeys": created } }));
                    return;
                }
            }
        }
        patch(&self.store, job_id, json!({
            "status": "approved",
            "data": { "issueKey": issue_key, "projectKey": project_key, "createdKeys": created },
            "result": { "summary": format!("Created {} subtask(s) on {issue_key}: {}.", created.len(), created.join(", ")), "files": [] },
        }));
    }
}

#[async_trait]
impl Feature for CreateJiraSubtasks {
    fn id(&self) -> &str {
        "create-jira-subtasks"
    }

    fn label(&self) -> &str {
        "Create Subtasks"
    }

    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        if !payload["issueKey"].is_string() {
            return Err(FeatureError::Other("create-jira-subtasks payload must include issueKey".into()));
        }
        let key = parse_issue_key(payload["issueKey"].as_str().unwrap_or(""))?;
        let subtasks = assert_subtask_rows(&payload)?;
        let job = self.store.create("create-jira-subtasks", Map::new());
        let id = job["id"].as_str().unwrap_or("").to_string();
        self.run_job(&id, &key.key, &key.project_key, &subtasks, &ctx.auth).await;
        Ok(self.store.get(&id).unwrap_or(job))
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        // Jobs from this feature never reach "awaiting-approval": this can only be a caller bug.
        Err(FeatureError::Other(format!("Cannot approve job in status \"{}\" \u{2014} create-jira-subtasks has no approve step", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::Other(format!("Cannot reject job in status \"{}\" \u{2014} create-jira-subtasks has no reject step", job["status"].as_str().unwrap_or(""))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        supports: bool,
        types: Vec<String>,
        issue: Result<Value, String>,
        fail_on: Option<usize>,
        no_key_on: Option<usize>,
        created: Mutex<Vec<NewSubtask>>,
    }

    fn fake() -> Fake {
        Fake { supports: true, types: vec!["Story".into(), "Epic".into()], issue: Ok(json!({ "fields": { "issuetype": { "name": "Story" }, "project": { "key": "PROJX" } } })), fail_on: None, no_key_on: None, created: Mutex::new(vec![]) }
    }

    #[async_trait]
    impl SubtaskApi for Fake {
        fn supports_subtasks(&self) -> bool {
            self.supports
        }
        fn parent_types(&self) -> Vec<String> {
            self.types.clone()
        }
        async fn get_issue_raw(&self, _: &AuthContext, _: &str, fields: &str) -> Result<Value, String> {
            assert_eq!(fields, "issuetype,project");
            self.issue.clone()
        }
        async fn create_subtask(&self, _: &AuthContext, s: &NewSubtask) -> Result<Option<String>, String> {
            let n = self.created.lock().unwrap().len();
            if self.fail_on == Some(n) {
                return Err("403 Forbidden".into());
            }
            self.created.lock().unwrap().push(s.clone());
            Ok((self.no_key_on != Some(n)).then(|| format!("PROJX-{}", 100 + n)))
        }
    }

    async fn run(f: Fake, payload: Value) -> (Result<Value, FeatureError>, Arc<Fake>, Arc<JobStore>) {
        let (store, api) = (Arc::new(JobStore::new()), Arc::new(f));
        let feature = create_jira_subtasks_feature(store.clone(), api.clone());
        (feature.start(payload, FeatureContext::default()).await, api, store)
    }

    fn rows(n: usize) -> Value {
        json!({ "issueKey": "projx-7", "subtasks": (0..n).map(|i| json!({ "summary": format!("Task {i}"), "assignee": "jdoe" })).collect::<Vec<_>>() })
    }

    #[tokio::test]
    async fn every_row_is_created_under_the_verified_ticket_and_the_job_ends_approved() {
        let (r, api, _) = run(fake(), rows(2)).await;
        let job = r.unwrap();
        assert_eq!(job["status"], "approved");
        assert_eq!(job["data"], json!({ "issueKey": "PROJX-7", "projectKey": "PROJX", "createdKeys": ["PROJX-100", "PROJX-101"] }));
        assert_eq!(job["result"]["summary"], "Created 2 subtask(s) on PROJX-7: PROJX-100, PROJX-101.");
        let made = api.created.lock().unwrap();
        assert_eq!((made[0].project_key.as_str(), made[0].parent_key.as_str(), made[0].summary.as_str(), made[0].assignee.as_str()), ("PROJX", "PROJX-7", "Task 0", "jdoe"));
    }

    #[tokio::test]
    async fn a_ticket_of_the_wrong_type_or_a_tracker_without_subtasks_fails_before_anything_is_created() {
        let wrong = Fake { issue: Ok(json!({ "fields": { "issuetype": { "name": "Bug" } } })), ..fake() };
        let (r, api, _) = run(wrong, rows(1)).await;
        assert_eq!(r.unwrap()["error"], "PROJX-7 is a Bug, not a Story or Epic \u{2014} subtasks can only be added to those.");
        assert!(api.created.lock().unwrap().is_empty());
        let unknown = Fake { issue: Ok(json!({})), ..fake() };
        assert!(run(unknown, rows(1)).await.0.unwrap()["error"].as_str().unwrap().starts_with("PROJX-7 is a ticket, not a"));
        assert_eq!(run(Fake { supports: false, ..fake() }, rows(1)).await.0.unwrap()["error"], "This issue tracker can't create subtasks.");
        let unreachable = Fake { issue: Err("HTTP 401".into()), ..fake() };
        assert_eq!(run(unreachable, rows(1)).await.0.unwrap()["error"], "HTTP 401");
    }

    #[tokio::test]
    async fn a_failure_part_way_names_what_was_already_created_and_the_project_falls_back_to_the_keys() {
        let (r, _, _) = run(Fake { fail_on: Some(1), ..fake() }, rows(3)).await;
        let job = r.unwrap();
        assert_eq!(job["status"], "failed");
        assert_eq!(job["error"], "403 Forbidden The following were already created before this failure: PROJX-100.");
        assert_eq!(job["data"]["createdKeys"], json!(["PROJX-100"]));
        let (r2, _, _) = run(Fake { fail_on: Some(0), ..fake() }, rows(1)).await;
        assert_eq!(r2.unwrap()["error"], "403 Forbidden");
        let no_project = Fake { issue: Ok(json!({ "fields": { "issuetype": { "name": "Epic" } } })), no_key_on: Some(0), ..fake() };
        let ok = run(no_project, rows(2)).await.0.unwrap();
        assert_eq!((ok["data"]["projectKey"].clone(), ok["data"]["createdKeys"].clone()), (json!("PROJX"), json!(["PROJX-101"])), "a reply without a key isn't counted");
    }

    #[tokio::test]
    async fn bad_payloads_are_refused_and_approve_and_reject_are_caller_bugs() {
        for bad in [json!(null), json!({}), json!({ "issueKey": 5 })] {
            assert_eq!(run(fake(), bad).await.0.unwrap_err().to_string(), "create-jira-subtasks payload must include issueKey");
        }
        assert!(run(fake(), json!({ "issueKey": "nope", "subtasks": [] })).await.0.unwrap_err().to_string().contains("doesn't look like a Jira issue key"));
        assert_eq!(run(fake(), json!({ "issueKey": "PROJX-7" })).await.0.unwrap_err().to_string(), "At least one subtask is required.");
        let f = create_jira_subtasks_feature(Arc::new(JobStore::new()), Arc::new(fake()));
        assert_eq!(f.approve(json!({ "status": "approved" }), FeatureContext::default()).await.unwrap_err().to_string(), "Cannot approve job in status \"approved\" \u{2014} create-jira-subtasks has no approve step");
        assert!(f.reject(json!({ "status": "x" }), FeatureContext::default()).await.unwrap_err().to_string().contains("no reject step"));
        assert_eq!((f.id(), f.label()), ("create-jira-subtasks", "Create Subtasks"));
    }
}
