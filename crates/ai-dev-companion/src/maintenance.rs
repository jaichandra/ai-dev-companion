//! What happens to a job that aged out (the Rust form of `cleanUpExpiredJob` and the scheduler's
//! `dailyMaintenance` in `server.ts`): its disposable worktree is removed and the expiry is noted
//! in the history, then the history is pruned past its retention window. Neither ever fails: a
//! problem is a log line.

use crate::git_cli::SystemGit;
use crate::history_db::HistoryDb;
use crate::history_record::{expired_event, HistoryRules};
use crate::jobs::JobStore;
use crate::worktree::{worktree_root, WorktreeHandle, Worktrees};
use crate::worktree_prune::{remove_pruned_worktree, Pruned};
use serde_json::{json, Value};
use std::path::Path;

/// The handle a job file records under `data.worktree` (a job file is untrusted: a missing field
/// is an empty one, and `remove_pruned_worktree` already refuses a dir outside the worktree root).
fn handle_of(v: &Value) -> WorktreeHandle {
    let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
    WorktreeHandle { id: text("id"), dir: text("dir"), repo_path: text("repoPath"), adopted: v["adopted"] == json!(true), branch: v["branch"].as_str().map(String::from) }
}

/// Removes the worktree of an expired job and records its expiry. Never fails.
pub async fn clean_up_expired_job(job: &Value, home: &Path, history: Option<&HistoryDb>, rules: &HistoryRules, log: &(dyn Fn(&str) + Sync)) {
    let id = job["id"].as_str().unwrap_or("");
    let handle = handle_of(&job["data"]["worktree"]);
    let failure = std::sync::Mutex::new(None::<String>);
    let how = remove_pruned_worktree(
        job,
        |_| async {
            let git = SystemGit;
            if let Err(e) = (Worktrees { git: &git, home }).remove_worktree(&handle).await {
                *failure.lock().unwrap() = Some(e);
            }
        },
        |dir| {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                *failure.lock().unwrap() = Some(e.to_string());
            }
        },
        &worktree_root(home).to_string_lossy(),
    )
    .await;
    match (failure.into_inner().unwrap(), how) {
        (Some(e), _) => log(&format!("[jobs] WARN - couldn't remove the worktree of expired job {id}: {e}")),
        (None, Pruned::Removed | Pruned::RemovedPlain) => log(&format!("[jobs] removed the worktree of expired job {id}")),
        _ => {}
    }
    if let Some(event) = expired_event(job, crate::job_files::now_ms() as i64, rules) {
        if let Some(h) = history {
            if let Err(e) = h.record_event_json(&event) {
                log(&format!("[history] WARN - couldn't record the expiry of job {id}: {e}"));
            }
        }
    }
}

/// The scheduler's "maintenance" task: expire stale jobs (removing their worktrees) and prune the
/// history past its retention. Its counts go into the run's events row.
pub async fn daily_maintenance(store: &JobStore, home: &Path, history: Option<&HistoryDb>, rules: &HistoryRules, retention_days: i64, log: &(dyn Fn(&str) + Sync)) -> Value {
    let mut expired = 0;
    for job in store.sweep_stale(crate::job_files::PRUNE_AFTER_MS, crate::job_files::now_ms()) {
        clean_up_expired_job(&job, home, history, rules, log).await;
        expired += 1;
    }
    let (items, events) = match history.map(|h| h.prune(retention_days, None)) {
        Some(Ok(p)) => p,
        Some(Err(e)) => {
            log(&format!("[maintenance] WARN - {e}"));
            return json!({ "outcome": "failed", "expired": expired, "error": e });
        }
        None => (0, 0),
    };
    if items > 0 || events > 0 {
        log(&format!("[history] pruned {items} item(s) and {events} event(s) past the retention window"));
    }
    json!({ "expired": expired, "prunedItems": items, "prunedEvents": events })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn logged() -> (Mutex<Vec<String>>, ) {
        (Mutex::new(vec![]),)
    }

    #[tokio::test]
    async fn a_job_without_a_worktree_only_gets_its_expiry_noted() {
        let d = tempfile::tempdir().unwrap();
        let (log,) = logged();
        let job = json!({ "id": "j1", "featureId": "analyze-issue", "status": "awaiting-approval", "createdAt": 1000, "data": {} });
        clean_up_expired_job(&job, d.path(), None, &HistoryRules::default(), &|m| log.lock().unwrap().push(m.into())).await;
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_compromised_worktree_is_deleted_as_a_plain_folder_and_one_outside_the_root_is_left() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_path_buf();
        let inside = worktree_root(&home).join("repo").join("job");
        std::fs::create_dir_all(&inside).unwrap();
        let outside = d.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let (log,) = logged();
        let make = |dir: &Path| json!({ "id": "j1", "featureId": "resolve-conflict", "status": "failed", "createdAt": 1000, "data": { "gitMetadataCompromised": true, "worktree": { "id": "j1", "dir": dir, "repoPath": "/repo" } } });
        clean_up_expired_job(&make(&inside), &home, None, &HistoryRules::default(), &|m| log.lock().unwrap().push(m.into())).await;
        clean_up_expired_job(&make(&outside), &home, None, &HistoryRules::default(), &|m| log.lock().unwrap().push(m.into())).await;
        assert!(!inside.exists() && outside.exists());
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines, ["[jobs] removed the worktree of expired job j1"]);
    }

    #[tokio::test]
    async fn the_daily_run_counts_nothing_when_there_is_nothing_stale_and_history_is_off() {
        let d = tempfile::tempdir().unwrap();
        let store = JobStore::new();
        let out = daily_maintenance(&store, d.path(), None, &HistoryRules::default(), 180, &|_| {}).await;
        assert_eq!(out, json!({ "expired": 0, "prunedItems": 0, "prunedEvents": 0 }));
    }

    #[tokio::test]
    async fn the_daily_run_prunes_the_history_past_its_retention() {
        let d = tempfile::tempdir().unwrap();
        let db = HistoryDb::open(&d.path().join("h.db"), crate::prereqs::GitRemoteRules::Github).unwrap();
        let (log,) = logged();
        let out = daily_maintenance(&JobStore::new(), d.path(), Some(&db), &HistoryRules::default(), 180, &|m| log.lock().unwrap().push(m.into())).await;
        assert_eq!(out["expired"], 0);
        assert!(out.get("outcome").is_none(), "{out}");
    }
}
