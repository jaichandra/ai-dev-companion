//! The "Claude edited a disposable worktree, a human reviewed the diff, now push
//! exactly that" plumbing shared by every feature that pushes a reviewed
//! worktree (port of `core/reviewed-push.ts`): resolve-conflict and
//! address-review-comments. Kept in one place so the two can't drift on the parts
//! that actually protect the user: the per-job lock, what "stage everything"
//! means, the tree fingerprint, and when a job may still be discarded.

use crate::exec::{git, ExecError};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use crate::reviewed_push_policy::can_reject;

/// Serializes approve and refresh-diff for the same job id: both stage the whole worktree,
/// fingerprint it, and mutate the job's status/data, so two concurrent calls for the same job
/// (a Refresh diff click racing an Approve click) must never interleave their git operations.
/// Calls for the SAME job run strictly one at a time, in order; calls for different job ids never
/// wait on each other.
#[derive(Default)]
pub struct JobLocks {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl JobLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn with_job_lock<T, F, Fut>(&self, job_id: &str, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let lock = self.locks.lock().unwrap().entry(job_id.to_string()).or_default().clone();
        let result = {
            // tokio's mutex queues waiters first-come first-served, so calls run in order.
            let _held = lock.lock().await;
            f().await
        };
        // Forget a job's lock once nobody else holds or waits on it, so the map doesn't grow
        // with every job id ever seen.
        let mut locks = self.locks.lock().unwrap();
        if Arc::strong_count(&lock) == 2 {
            locks.remove(job_id);
        }
        result
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.locks.lock().unwrap().len()
    }
}

/// Stages the ENTIRE worktree (no pathspec list, so no glob/pathspec edge case, such as a
/// rename, a space or a quoted path, can be gotten wrong), then unstages the node_modules symlink
/// that `link_node_modules` creates: a gitignore directory pattern like "/node_modules/" does NOT
/// match a symlink (only a real directory), so `-A` always picks it up as untracked. Checking
/// existence first means a REAL reset failure is never swallowed; it fails with git's own
/// actionable stderr instead.
pub async fn stage_all(worktree_dir: &Path) -> Result<(), ExecError> {
    git(&["add", "-A"], worktree_dir, false, None).await?;
    if worktree_dir.join("node_modules").symlink_metadata().is_ok() {
        git(&["reset", "-q", "--", "node_modules"], worktree_dir, false, None).await?;
    }
    Ok(())
}

/// The worktree's current tree hash: a fingerprint of the ENTIRE tree (not just whichever files
/// were last staged), taken right after `stage_all` so it reflects everything in the worktree.
pub async fn write_tree(worktree_dir: &Path) -> Result<String, ExecError> {
    Ok(git(&["write-tree"], worktree_dir, false, None).await?.stdout.trim().to_string())
}

/// Approve must push exactly what the panel last showed. Comparing the WHOLE tree's hash catches
/// any change since the last render, anywhere in the worktree.
pub fn assert_tree_matches_reviewed(tree: &str, reviewed_tree: Option<&str>) -> Result<(), String> {
    if Some(tree) != reviewed_tree {
        return Err("The worktree changed since you reviewed it \u{2014} click Refresh diff, check the changes, then approve.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::git as sh;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        sh(d.path(), &["init", "-q"]);
        d
    }

    #[test]
    fn can_reject_is_the_shared_policy() {
        assert!(can_reject("awaiting-approval") && can_reject("failed"));
        assert!(!can_reject("approving") && !can_reject("rejecting"));
    }

    #[tokio::test]
    async fn calls_for_one_job_run_one_at_a_time_in_order_and_other_jobs_do_not_wait() {
        let locks = Arc::new(JobLocks::new());
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let running = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for i in 0..4 {
            let (l, log, running) = (locks.clone(), log.clone(), running.clone());
            handles.push(tokio::spawn(async move {
                l.with_job_lock("job-a", || async move {
                    assert_eq!(running.fetch_add(1, Ordering::SeqCst), 0, "two calls for the same job overlapped");
                    log.lock().unwrap().push(format!("start {i}"));
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    log.lock().unwrap().push(format!("end {i}"));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // A different job is not held up by job-a's queue.
        let other = tokio::time::timeout(Duration::from_millis(10), locks.with_job_lock("job-b", || async { 7 })).await;
        assert_eq!(other, Ok(7));
        for h in handles {
            h.await.unwrap();
        }
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 8);
        for pair in log.chunks(2) {
            assert!(pair[0].starts_with("start") && pair[1].starts_with("end") && pair[0][6..] == pair[1][4..], "{log:?}");
        }
        assert_eq!(log.iter().filter(|l| l.starts_with("start")).cloned().collect::<Vec<_>>(), ["start 0", "start 1", "start 2", "start 3"], "in order");
        assert_eq!(locks.tracked(), 0, "finished jobs leave nothing behind");
    }

    #[tokio::test]
    async fn a_failing_call_does_not_block_the_next_and_returns_its_own_result() {
        let locks = JobLocks::new();
        let r: Result<(), String> = locks.with_job_lock("j", || async { Err("boom".into()) }).await;
        assert_eq!(r, Err("boom".to_string()));
        assert_eq!(locks.with_job_lock("j", || async { 1 }).await, 1);
    }

    #[tokio::test]
    async fn staging_takes_everything_but_the_node_modules_symlink_and_the_tree_hash_changes_with_content() {
        let d = repo();
        let p = d.path();
        std::fs::write(p.join("a.txt"), "one\n").unwrap();
        std::fs::write(p.join("with space.txt"), "x\n").unwrap();
        std::fs::create_dir(p.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(p.join("elsewhere"), p.join("node_modules")).unwrap();
        stage_all(p).await.unwrap();
        let staged = sh(p, &["diff", "--cached", "--name-only"]);
        assert_eq!(staged, "a.txt\nwith space.txt");
        let first = write_tree(p).await.unwrap();
        assert_eq!(first.len(), 40);
        assert_eq!(write_tree(p).await.unwrap(), first, "stable for the same content");
        std::fs::write(p.join("a.txt"), "two\n").unwrap();
        stage_all(p).await.unwrap();
        assert_ne!(write_tree(p).await.unwrap(), first);
    }

    #[tokio::test]
    async fn staging_without_node_modules_skips_the_reset_and_a_real_failure_is_not_swallowed() {
        let d = repo();
        std::fs::write(d.path().join("a.txt"), "x\n").unwrap();
        stage_all(d.path()).await.unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(stage_all(plain.path()).await.is_err(), "not a git repository");
        assert!(write_tree(plain.path()).await.is_err());
    }

    #[test]
    fn approve_is_refused_unless_the_tree_is_exactly_the_reviewed_one() {
        assert_eq!(assert_tree_matches_reviewed("abc", Some("abc")), Ok(()));
        let msg = "The worktree changed since you reviewed it \u{2014} click Refresh diff, check the changes, then approve.";
        assert_eq!(assert_tree_matches_reviewed("abc", Some("def")), Err(msg.into()));
        assert_eq!(assert_tree_matches_reviewed("abc", None), Err(msg.into()));
    }
}
