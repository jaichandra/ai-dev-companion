//! Provisions and locates each repo's dedicated PR-review worktree (port of
//! `core/review-worktree.js`): a persistent fixture at
//! `<stateDir>/<repo>.worktrees/pr-review`, not beside the repo's own clone.
//!
//! This is a different lifecycle from the per-job worktrees: those are rooted
//! under `<stateDir>/worktrees`, detached by design, and removed on
//! approve/reject. This worktree is provisioned once per repo and never
//! removed; review-in-editor re-checks it out to a different branch on every
//! click instead of creating a new one each time.

use crate::git_cli::{Git, GitOpts, FETCH_TIMEOUT};
use crate::js_text::js_trim;
use crate::paths::repo_worktrees_root;
use std::fs;
use std::path::{Path, PathBuf};

/// `<stateDir>/<basename>.worktrees/pr-review`.
pub fn review_worktree_path(repo_path: &Path, home: &Path) -> PathBuf {
    repo_worktrees_root(repo_path, home).join("pr-review")
}

fn real(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(crate::node_path::resolve(&p.to_string_lossy())))
}

/// True if `dir` is already registered as one of `repo_path`'s own worktrees,
/// regardless of what branch it's on or whether it's dirty. The only thing
/// that matters for idempotency: a worktree the user already checked out by
/// hand (say mid-review) must never be recreated out from under them.
pub async fn is_registered_worktree(repo_path: &Path, dir: &Path, git: &impl Git) -> bool {
    let Ok(out) = git.run(&["worktree", "list", "--porcelain"], repo_path, GitOpts::allow_failure()).await else { return false };
    let target = real(dir);
    out.stdout.split('\n').filter_map(|l| l.strip_prefix("worktree ")).any(|c| {
        let candidate = Path::new(js_trim(c));
        candidate.exists() && real(candidate) == target
    })
}

/// `git worktree add` never gives you `node_modules` (it's gitignored, and each
/// worktree is a separate checkout). Symlinked, not copied (a real clone's can
/// be well over 1 GB), so editor tooling works in the review worktree.
pub fn link_node_modules(repo_path: &Path, worktree_dir: &Path) {
    let (source, dest) = (repo_path.join("node_modules"), worktree_dir.join("node_modules"));
    if source.exists() && !dest.exists() {
        let _ = std::os::unix::fs::symlink(source, dest);
    }
}

/// The ref to land a fresh review worktree on before any real PR branch is
/// checked out into it: origin/HEAD (the repo's actual default branch) if the
/// clone knows it, else the two conventional fallbacks.
async fn default_remote_ref(repo_path: &Path, git: &impl Git) -> Result<String, String> {
    let symbolic = git.run(&["symbolic-ref", "refs/remotes/origin/HEAD"], repo_path, GitOpts::allow_failure()).await?;
    let name = js_trim(&symbolic.stdout);
    if symbolic.code == 0 && !name.is_empty() {
        return Ok(name.strip_prefix("refs/remotes/").unwrap_or(name).to_string());
    }
    for candidate in ["origin/master", "origin/main"] {
        if git.run(&["rev-parse", "--verify", candidate], repo_path, GitOpts::allow_failure()).await?.code == 0 {
            return Ok(candidate.to_string());
        }
    }
    Err(format!("Could not determine a default branch for {} (no origin/HEAD, origin/master, or origin/main).", repo_path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    pub dir: PathBuf,
    pub created: bool,
}

/// Idempotent: a no-op if `review_worktree_path` is already a registered
/// worktree of this repo, on whatever branch it happens to be on right now
/// (review-in-editor's own start re-checks it out to the PR's branch on every
/// click, so provisioning only ever needs to get a valid worktree into
/// existence once).
pub async fn provision_review_worktree(repo_path: &Path, home: &Path, git: &impl Git) -> Result<Provisioned, String> {
    let dir = review_worktree_path(repo_path, home);
    if is_registered_worktree(repo_path, &dir, git).await {
        return Ok(Provisioned { dir, created: false });
    }
    git.run(&["fetch", "origin", "--prune"], repo_path, GitOpts::timeout(FETCH_TIMEOUT)).await?;
    let reference = default_remote_ref(repo_path, git).await?;
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // A stale, non-worktree leftover at this exact path (a directory from a
    // previous failed run) would make `git worktree add` fail with a confusing
    // "already exists" error: clear it first.
    if dir.exists() {
        let _ = fs::remove_dir_all(&dir);
    }
    // The directory may have been deleted out from under git (cleanup, purge of
    // the data folder) while `.git/worktrees/<id>` still registers it:
    // `worktree add` then refuses with "missing but already registered".
    git.run(&["worktree", "prune"], repo_path, GitOpts::default()).await?;
    git.run(&["worktree", "add", "--detach", &dir.to_string_lossy(), &reference], repo_path, GitOpts::default()).await?;
    link_node_modules(repo_path, &dir);
    Ok(Provisioned { dir, created: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::{git as sh, make_repos};
    use crate::git_cli::SystemGit;

    #[test]
    fn review_worktree_path_is_state_dir_repo_worktrees_pr_review() {
        let p = review_worktree_path(Path::new("/Users/x/gitviews/sample-app"), Path::new("/h"));
        assert_eq!(p, Path::new("/h").join(format!(".{}", crate::app_identity::app_slug())).join("sample-app.worktrees").join("pr-review"));
    }

    #[tokio::test]
    async fn is_registered_worktree_is_false_before_anything_has_been_provisioned() {
        let r = make_repos();
        assert!(!is_registered_worktree(&r.clone, &review_worktree_path(&r.clone, &r.home), &SystemGit).await);
    }

    #[tokio::test]
    async fn provision_creates_a_real_worktree_the_first_time() {
        let r = make_repos();
        let p = provision_review_worktree(&r.clone, &r.home, &SystemGit).await.unwrap();
        assert!(p.created);
        assert_eq!(p.dir, review_worktree_path(&r.clone, &r.home));
        assert!(p.dir.join("README.md").exists());
        assert!(is_registered_worktree(&r.clone, &p.dir, &SystemGit).await);
    }

    #[tokio::test]
    async fn provision_is_idempotent_and_leaves_an_off_branch_worktree_alone() {
        let r = make_repos();
        let first = provision_review_worktree(&r.clone, &r.home, &SystemGit).await.unwrap();
        // The user (or a real review) has since checked out a real local branch by hand.
        sh(&first.dir, &["checkout", "-q", "-B", "some-review-branch"]);
        let second = provision_review_worktree(&r.clone, &r.home, &SystemGit).await.unwrap();
        assert_eq!((second.created, &second.dir), (false, &first.dir));
        assert_eq!(sh(&first.dir, &["branch", "--show-current"]), "some-review-branch");
    }

    #[tokio::test]
    async fn provision_recovers_when_the_directory_was_deleted_but_git_still_registers_it() {
        let r = make_repos();
        let first = provision_review_worktree(&r.clone, &r.home, &SystemGit).await.unwrap();
        fs::remove_dir_all(&first.dir).unwrap();
        let second = provision_review_worktree(&r.clone, &r.home, &SystemGit).await.unwrap();
        assert!(second.created && second.dir.exists());
    }

    #[tokio::test]
    async fn a_stray_non_worktree_folder_at_the_path_is_cleared_and_node_modules_is_linked() {
        let r = make_repos();
        fs::create_dir_all(r.clone.join("node_modules")).unwrap();
        let dir = review_worktree_path(&r.clone, &r.home);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("junk.txt"), "x").unwrap();
        let p = provision_review_worktree(&r.clone, &r.home, &SystemGit).await.unwrap();
        assert!(p.created && !p.dir.join("junk.txt").exists());
        assert!(fs::symlink_metadata(p.dir.join("node_modules")).unwrap().file_type().is_symlink());
    }
}
