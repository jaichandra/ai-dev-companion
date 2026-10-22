//! A read-only checkout of a repo's latest default branch for Analyze issue
//! (port of `core/analysis-checkout.js`). It lives under the state dir
//! (`<stateDir>/repos/<PROJECT>/<repo>`), never in the user's own folders: the
//! analysis reads the current `origin/<default>` instead of whatever branch and
//! half-finished edits the user's clone has, and leaves that clone, and every
//! `<repo>.worktrees` folder beside it, alone.
//!
//! First use makes a local clone of the user's clone (object files are
//! hard-linked, so it is quick and small), points its origin at the real
//! remote and fetches. Later uses just fetch and move the detached checkout.

use crate::git_cli::{Git, GitOpts, FETCH_TIMEOUT};
use crate::js_text::{js_trim, slice_utf16};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());

/// `<root>/<PROJECT>/<repo>` for a "PROJECT/repo" key, or `None` for anything else.
pub fn checkout_path(root: &Path, repo_key: Option<&str>) -> Option<PathBuf> {
    let parts: Vec<&str> = repo_key.unwrap_or("").split('/').collect();
    (parts.len() == 2 && parts.iter().all(|p| NAME_RE.is_match(p))).then(|| root.join(parts[0]).join(parts[1]))
}

// One refresh of a checkout at a time: two analyses of the same repo must not
// fetch and check out into the same folder at once.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_for(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    LOCKS.lock().unwrap().entry(dir.to_path_buf()).or_default().clone()
}

async fn origin_head_of(git: &impl Git, dir: &Path) -> Result<String, String> {
    let head = git.run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], dir, GitOpts::allow_failure()).await?;
    let name = js_trim(&head.stdout);
    Ok(if head.code == 0 { name.strip_prefix("origin/").unwrap_or(name).to_string() } else { String::new() })
}

async fn has_remote_branch(git: &impl Git, dir: &Path, name: &str) -> Result<bool, String> {
    if name.is_empty() {
        return Ok(false);
    }
    Ok(git.run(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{name}")], dir, GitOpts::allow_failure()).await?.code == 0)
}

/// The remote's default branch. A local clone of the user's clone inherits that
/// clone's checked-out branch as its origin/HEAD, so it is not trusted until
/// `remote set-head --auto` has asked the real remote. Failing that, the user's
/// own clone's origin/HEAD (set when it was cloned), then master/main.
async fn default_branch_of(git: &impl Git, dir: &Path, user_clone: &Path, asked: bool) -> Result<Option<String>, String> {
    if asked {
        let name = origin_head_of(git, dir).await?;
        if has_remote_branch(git, dir, &name).await? {
            return Ok(Some(name));
        }
    }
    let theirs = origin_head_of(git, user_clone).await?;
    if has_remote_branch(git, dir, &theirs).await? {
        return Ok(Some(theirs));
    }
    for candidate in ["master", "main"] {
        if has_remote_branch(git, dir, candidate).await? {
            return Ok(Some(candidate.to_string()));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub dir: PathBuf,
    pub branch: String,
    /// False when the fetch failed (offline, no access) and the checkout is as
    /// of the last time it worked.
    pub fresh: bool,
}

/// Brings the checkout for `repo_key` up to date. Errors when there is no
/// usable checkout at all.
pub async fn ensure_analysis_checkout(user_clone: &Path, repo_key: &str, root: &Path, git: &impl Git) -> Result<Checkout, String> {
    let dir = checkout_path(root, Some(repo_key)).ok_or_else(|| format!("Not a repo key: {}", slice_utf16(repo_key, 80)))?;
    let lock = lock_for(&dir);
    let _turn = lock.lock().await;
    let existed = dir.join(".git").exists();
    let parent = dir.parent().unwrap_or(root).to_path_buf();
    let slow = GitOpts::timeout(FETCH_TIMEOUT);
    if !existed {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let remote = git.run(&["remote", "get-url", "origin"], user_clone, GitOpts::allow_failure()).await?;
        git.run(&["clone", "--quiet", "--no-checkout", &user_clone.to_string_lossy(), &dir.to_string_lossy()], &parent, slow).await?;
        let url = if remote.code == 0 { js_trim(&remote.stdout) } else { "" };
        if !url.is_empty() {
            git.run(&["remote", "set-url", "origin", url], &dir, GitOpts::default()).await?;
        }
    }

    let mut fresh = true;
    if let Err(e) = git.run(&["fetch", "--quiet", "--prune", "origin"], &dir, slow).await {
        fresh = false;
        if existed {
            eprintln!("[analysis-checkout] fetch of {repo_key} failed, using the last copy: {e}");
        } else {
            // A local clone of the user's clone still has its origin/* refs; usable, just as old as that clone.
            eprintln!("[analysis-checkout] first fetch of {repo_key} failed: {e}");
        }
    }

    let mut asked = false;
    if fresh {
        asked = git.run(&["remote", "set-head", "origin", "--auto"], &dir, slow.with_allow_failure(true)).await?.code == 0;
    }
    let Some(branch) = default_branch_of(git, &dir, user_clone, asked).await? else { return Err(format!("No origin/master or origin/main in the checkout of {repo_key}.")) };
    git.run(&["checkout", "--quiet", "--force", "--detach", &format!("origin/{branch}")], &dir, slow).await?;
    Ok(Checkout { dir, branch, fresh })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::{git as sh, make_repos, Repos};
    use crate::git_cli::SystemGit;

    fn checkouts(r: &Repos) -> PathBuf {
        r.root.join("state").join("repos")
    }

    async fn ensure(r: &Repos) -> Result<Checkout, String> {
        ensure_analysis_checkout(&r.clone, "ACME/sample-app", &checkouts(r), &SystemGit).await
    }

    fn read(dir: &Path, f: &str) -> String {
        fs::read_to_string(dir.join(f)).unwrap()
    }

    #[test]
    fn checkout_path_accepts_project_repo_only_and_stays_under_the_root() {
        assert_eq!(checkout_path(Path::new("/s/repos"), Some("ACME/sample-app")), Some(PathBuf::from("/s/repos/ACME/sample-app")));
        for bad in [Some("sample-app"), Some("a/b/c"), Some("../x"), Some("A/.."), Some("A/."), Some("/etc"), Some("A/b c"), Some(""), None] {
            assert_eq!(checkout_path(Path::new("/s/repos"), bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_first_use_makes_a_detached_checkout_of_origin_master_apart_from_the_users_clone() {
        let r = make_repos();
        let out = ensure(&r).await.unwrap();
        assert_eq!(out.dir, checkouts(&r).join("ACME").join("sample-app"));
        assert_eq!((out.branch.as_str(), out.fresh), ("master", true));
        assert_eq!(read(&out.dir, "README.md"), "hi\n");
        assert_eq!(sh(&out.dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD");
        assert_eq!(sh(&out.dir, &["remote", "get-url", "origin"]), r.origin.to_str().unwrap());
        assert!(!PathBuf::from(format!("{}.worktrees", r.clone.display())).exists());
    }

    #[tokio::test]
    async fn a_later_use_moves_to_the_newest_origin_master_and_ignores_the_users_own_branch_and_edits() {
        let r = make_repos();
        ensure(&r).await.unwrap();
        // The user is on a feature branch with an uncommitted edit; master moves on at origin.
        sh(&r.clone, &["checkout", "-q", "-b", "feature/x"]);
        fs::write(r.clone.join("README.md"), "user edit\n").unwrap();
        let other = r.root.join("other");
        sh(&r.root, &["clone", "-q", r.origin.to_str().unwrap(), "other"]);
        sh(&other, &["config", "user.email", "t@example.com"]);
        sh(&other, &["config", "user.name", "T"]);
        fs::write(other.join("README.md"), "two\n").unwrap();
        sh(&other, &["commit", "-q", "-am", "second"]);
        sh(&other, &["push", "-q", "origin", "master"]);

        let out = ensure(&r).await.unwrap();
        assert!(out.fresh);
        assert_eq!(read(&out.dir, "README.md"), "two\n");
        assert_eq!(read(&r.clone, "README.md"), "user edit\n");
        assert_eq!(sh(&r.clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/x");
    }

    #[tokio::test]
    async fn it_follows_the_remotes_default_branch_even_when_the_users_clone_sits_on_another_branch() {
        let r = make_repos();
        sh(&r.clone, &["checkout", "-q", "-b", "feature/x"]);
        fs::write(r.clone.join("README.md"), "feature\n").unwrap();
        sh(&r.clone, &["commit", "-q", "-am", "feature work"]);
        sh(&r.clone, &["push", "-q", "origin", "feature/x"]);
        let out = ensure(&r).await.unwrap();
        assert_eq!(out.branch, "master");
        assert_eq!(read(&out.dir, "README.md"), "hi\n");
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_last_copy_and_says_it_is_not_fresh() {
        let r = make_repos();
        ensure(&r).await.unwrap();
        fs::rename(&r.origin, format!("{}.gone", r.origin.display())).unwrap();
        let out = ensure(&r).await.unwrap();
        assert!(!out.fresh);
        assert_eq!(read(&out.dir, "README.md"), "hi\n");
    }

    #[tokio::test]
    async fn two_refreshes_of_the_same_checkout_at_once_run_one_after_the_other() {
        let r = make_repos();
        let (a, b) = tokio::join!(ensure(&r), ensure(&r));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.dir, b.dir);
        assert_eq!(read(&a.dir, "README.md"), "hi\n");
    }

    #[tokio::test]
    async fn a_bad_repo_key_is_refused_before_anything_is_created() {
        let e = ensure_analysis_checkout(Path::new("/nope"), "../x", Path::new("/nope/repos"), &SystemGit).await.unwrap_err();
        assert!(e.contains("Not a repo key"), "{e}");
        assert!(!Path::new("/nope").exists());
    }
}
