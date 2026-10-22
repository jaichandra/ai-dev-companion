//! Removes the worktree of a job that aged out (port of
//! `core/worktree-prune.js`). The caller supplies the real removers so this
//! stays testable. A job flagged `gitMetadataCompromised` (resolve-conflict
//! found the worktree's `.git` redirected) must never be handed to git again:
//! it is deleted as a plain directory instead, exactly as that feature's own
//! discard does.

use crate::session_cwd::strictly_inside;
use serde_json::Value;
use std::future::Future;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pruned {
    None,
    Skipped,
    RemovedPlain,
    Removed,
}

pub async fn remove_pruned_worktree<F, Fut>(job: &Value, remove_worktree: F, rm_dir: impl Fn(&str), worktree_root: &str) -> Pruned
where
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = ()>,
{
    let handle = &job["data"]["worktree"];
    if !handle.is_object() {
        return Pruned::None;
    }
    // Defence in depth: load_jobs already checked this, but never delete outside the root.
    let dir = handle["dir"].as_str();
    if !strictly_inside(Some(worktree_root), dir) {
        return Pruned::Skipped;
    }
    if crate::js_text::js_truthy(job["data"].get("gitMetadataCompromised")) {
        rm_dir(dir.unwrap_or_default());
        return Pruned::RemovedPlain;
    }
    remove_worktree(handle.clone()).await;
    Pruned::Removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    async fn run(job: Value) -> (Pruned, Vec<String>) {
        let log = RefCell::new(vec![]);
        let r = remove_pruned_worktree(&job, |h| { let log = &log; async move { log.borrow_mut().push(format!("git:{}", h["dir"].as_str().unwrap())); } }, |d| log.borrow_mut().push(format!("rm:{d}")), "/wt").await;
        (r, log.into_inner())
    }

    #[tokio::test]
    async fn a_job_without_a_worktree_is_left_alone() {
        assert_eq!(run(json!({})).await, (Pruned::None, vec![]));
        assert_eq!(run(json!({ "data": { "worktree": "x" } })).await, (Pruned::None, vec![]));
    }

    #[tokio::test]
    async fn a_worktree_outside_the_root_is_never_touched() {
        assert_eq!(run(json!({ "data": { "worktree": { "dir": "/elsewhere/x" } } })).await, (Pruned::Skipped, vec![]));
        assert_eq!(run(json!({ "data": { "worktree": { "dir": "/wt" } } })).await.0, Pruned::Skipped, "the root itself is not inside it");
        assert_eq!(run(json!({ "data": { "worktree": {} } })).await.0, Pruned::Skipped);
    }

    #[tokio::test]
    async fn an_ordinary_worktree_goes_through_git_and_a_compromised_one_is_deleted_as_a_plain_directory() {
        let w = json!({ "dir": "/wt/a", "repoPath": "/r" });
        assert_eq!(run(json!({ "data": { "worktree": w } })).await, (Pruned::Removed, vec!["git:/wt/a".to_string()]));
        assert_eq!(run(json!({ "data": { "worktree": w, "gitMetadataCompromised": true } })).await, (Pruned::RemovedPlain, vec!["rm:/wt/a".to_string()]));
    }
}
