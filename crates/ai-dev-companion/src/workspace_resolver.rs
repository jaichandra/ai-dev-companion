//! Where should a feature work on a branch? (port of
//! `core/workspace-resolver.js`.) Look for something that already exists before
//! making anything: first a worktree (or the clone itself) that has the branch
//! checked out, then a local or origin branch of that name. Nothing here
//! creates a worktree unless `create_branch_worktree` is called, and the caller
//! does that only after the person confirms.

use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::git_cli::{Git, GitOpts, FETCH_TIMEOUT};
use crate::js_text::{js_trim, slice_utf16};
use crate::node_path;
use crate::paths::repo_worktrees_root;
use crate::review_worktree::link_node_modules;
use crate::ticket_git::{count_changed, key_pattern, parse_for_each_ref, parse_worktree_porcelain, FOR_EACH_REF_FORMAT};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Operations that leave a worktree half-way through something; git keeps a
/// marker for each under the worktree's own git dir.
const IN_PROGRESS_MARKERS: [(&str, &str); 5] = [("MERGE_HEAD", "merge"), ("rebase-merge", "rebase"), ("rebase-apply", "rebase"), ("CHERRY_PICK_HEAD", "cherry-pick"), ("REVERT_HEAD", "revert")];

fn real(p: &str) -> String {
    crate::worktree_registry::real(p)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    pub dirty: bool,
    pub changed: usize,
    pub in_progress: Option<&'static str>,
}

/// Uncommitted changes and any half-finished merge/rebase in `dir`.
pub async fn inspect_worktree(dir: &Path, git: &impl Git) -> Result<Inspection, String> {
    let st = git.run(&["status", "--porcelain"], dir, GitOpts::allow_failure()).await?;
    let changed = if st.code == 0 { count_changed(&st.stdout) } else { 0 };
    let mut in_progress = None;
    for (marker, name) in IN_PROGRESS_MARKERS {
        let p = git.run(&["rev-parse", "--git-path", marker], dir, GitOpts::allow_failure()).await?;
        if p.code != 0 {
            continue;
        }
        let file = node_path::resolve(&format!("{}/{}", dir.display(), js_trim(&p.stdout)));
        let file = if node_path::is_absolute(js_trim(&p.stdout)) { node_path::resolve(js_trim(&p.stdout)) } else { file };
        if Path::new(&file).exists() {
            in_progress = Some(name);
            break;
        }
    }
    Ok(Inspection { dirty: changed > 0, changed, in_progress })
}

async fn ref_exists(repo_path: &Path, reference: &str, git: &impl Git) -> Result<bool, String> {
    Ok(git.run(&["show-ref", "--verify", "--quiet", reference], repo_path, GitOpts::allow_failure()).await?.code == 0)
}

/// The branch names to consider: the one asked for, else every branch whose
/// name carries the ticket key.
async fn candidate_branches(repo_path: &Path, branch: Option<&str>, issue_key: Option<&str>, git: &impl Git) -> Result<Vec<String>, String> {
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        if !is_safe_branch_name(branch) {
            return Err(format!("Not a safe branch name: {}", slice_utf16(branch, 80)));
        }
        return Ok(vec![branch.to_string()]);
    }
    let Some(key) = issue_key.filter(|k| !k.is_empty()) else { return Err("resolveWorkspace needs a branch or an issue key.".into()) };
    key_pattern(key)?; // validates the key
    let format = format!("--format={FOR_EACH_REF_FORMAT}");
    let refs = git.run(&["for-each-ref", &format, "refs/heads", "refs/remotes/origin"], repo_path, GitOpts::default()).await?;
    Ok(parse_for_each_ref(&refs.stdout, key)?.into_iter().map(|b| b.name).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspace {
    /// An existing checkout of the branch (any path, the clone included);
    /// `usable` is false when it has uncommitted changes or a merge/rebase in
    /// progress, and `reason` says which.
    Worktree { dir: String, branch: String, is_main_clone: bool, state: Inspection, usable: bool, reason: Option<String>, alternatives: Vec<String> },
    /// The branch exists but is checked out nowhere; making a worktree for it
    /// needs the person's confirmation.
    Branch { branch: String, local: bool, remote: bool, alternatives: Vec<String> },
    /// No such branch here (or, for a key, no branch at all).
    None { branch: Option<String> },
}

impl Workspace {
    pub fn to_json(&self) -> Value {
        match self {
            Workspace::Worktree { dir, branch, is_main_clone, state, usable, reason, alternatives } => json!({
                "kind": "worktree", "dir": dir, "branch": branch, "isMainClone": is_main_clone,
                "dirty": state.dirty, "changed": state.changed, "inProgress": state.in_progress,
                "usable": usable, "reason": reason, "alternatives": alternatives,
            }),
            Workspace::Branch { branch, local, remote, alternatives } => json!({ "kind": "branch", "branch": branch, "local": local, "remote": remote, "alternatives": alternatives }),
            Workspace::None { branch } => json!({ "kind": "none", "branch": branch, "alternatives": [] }),
        }
    }
}

/// Finds the place to work on a branch (the PR's source branch) or, with only
/// an issue key, on the ticket's branches. Never creates or changes anything.
pub async fn resolve_workspace(repo_path: &Path, branch: Option<&str>, issue_key: Option<&str>, git: &impl Git) -> Result<Workspace, String> {
    let names = candidate_branches(repo_path, branch, issue_key, git).await?;
    let list = git.run(&["worktree", "list", "--porcelain"], repo_path, GitOpts::default()).await?;
    let main = real(&repo_path.to_string_lossy());
    let worktrees: Vec<_> = parse_worktree_porcelain(&list.stdout).into_iter().filter(|wt| wt.branch.is_some() && Path::new(&wt.dir).exists()).collect();
    let others = |name: &str| names.iter().filter(|n| *n != name).cloned().collect::<Vec<_>>();

    for name in &names {
        let Some(wt) = worktrees.iter().find(|w| w.branch.as_deref() == Some(name)) else { continue };
        let state = inspect_worktree(Path::new(&wt.dir), git).await?;
        let reason = if let Some(p) = state.in_progress {
            Some(format!("a {p} is in progress in {}", wt.dir))
        } else if state.dirty {
            Some(format!("{} has {} uncommitted change{}", wt.dir, state.changed, if state.changed == 1 { "" } else { "s" }))
        } else {
            None
        };
        return Ok(Workspace::Worktree { dir: wt.dir.clone(), branch: name.clone(), is_main_clone: real(&wt.dir) == main, usable: reason.is_none(), reason, state, alternatives: others(name) });
    }

    for name in &names {
        let local = ref_exists(repo_path, &format!("refs/heads/{name}"), git).await?;
        let remote = ref_exists(repo_path, &format!("refs/remotes/origin/{name}"), git).await?;
        if local || remote {
            return Ok(Workspace::Branch { branch: name.clone(), local, remote, alternatives: others(name) });
        }
    }
    Ok(Workspace::None { branch: branch.filter(|b| !b.is_empty()).map(String::from) })
}

/// `<repo>.worktrees/<branch with / and other odd characters as ->`.
pub fn branch_worktree_path(repo_path: &Path, branch: &str, home: &Path) -> PathBuf {
    let mut name = String::new();
    for c in branch.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            name.push(c);
        } else {
            // One `-` per UTF-16 unit, as the Node regex replaces.
            name.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    repo_worktrees_root(repo_path, home).join(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchWorktree {
    pub dir: PathBuf,
    pub branch: String,
    pub created: bool,
}

/// Makes a worktree for an existing local or origin branch. Call it only after
/// the person confirmed (`resolve_workspace` said `Branch`). It never makes a
/// new branch: starting one for a ticket is `ensure_ticket_worktree`'s job. A
/// folder already at the path is never touched.
pub async fn create_branch_worktree(repo_path: &Path, branch: &str, home: &Path, git: &impl Git) -> Result<BranchWorktree, String> {
    if !is_safe_branch_name(branch) {
        return Err(format!("Not a safe branch name: {}", slice_utf16(branch, 80)));
    }
    git.run(&["check-ref-format", "--branch", branch], repo_path, GitOpts::default()).await?;
    let dir = branch_worktree_path(repo_path, branch, home);
    if dir.exists() {
        return Err(format!("{} already exists. Move it aside and try again.", dir.display()));
    }
    let had_local = ref_exists(repo_path, &format!("refs/heads/{branch}"), git).await?;
    git.run(&["fetch", "origin", &format!("refs/heads/{branch}:refs/remotes/origin/{branch}")], repo_path, GitOpts::timeout(FETCH_TIMEOUT).with_allow_failure(had_local)).await?;
    let has_remote = ref_exists(repo_path, &format!("refs/remotes/origin/{branch}"), git).await?;
    if !had_local && !has_remote {
        return Err(format!("Branch {branch} doesn't exist locally or on origin."));
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let dir_str = dir.to_string_lossy().into_owned();
    if had_local {
        git.run(&["worktree", "add", &dir_str, branch], repo_path, GitOpts::default()).await?;
    } else {
        git.run(&["worktree", "add", "--track", "-b", branch, &dir_str, &format!("origin/{branch}")], repo_path, GitOpts::default()).await?;
    }
    link_node_modules(repo_path, &dir);
    Ok(BranchWorktree { dir, branch: branch.to_string(), created: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::{git as sh, make_repos_with, Repos};
    use crate::git_cli::SystemGit;

    fn repos() -> Repos {
        make_repos_with(&["feature/PROJ-7-login", "feature/PROJ-7-logout"])
    }

    fn worktree_count(clone: &Path) -> usize {
        sh(clone, &["worktree", "list", "--porcelain"]).split('\n').filter(|l| l.starts_with("worktree ")).count()
    }

    async fn resolve(r: &Repos, branch: Option<&str>, key: Option<&str>) -> Workspace {
        resolve_workspace(&r.clone, branch, key, &SystemGit).await.unwrap()
    }

    #[tokio::test]
    async fn a_branch_checked_out_in_the_clone_itself_is_the_first_choice() {
        let r = repos();
        sh(&r.clone, &["checkout", "-q", "-b", "feature/PROJ-7-login", "--track", "origin/feature/PROJ-7-login"]);
        match resolve(&r, Some("feature/PROJ-7-login"), None).await {
            Workspace::Worktree { dir, is_main_clone, usable, .. } => assert_eq!((dir, is_main_clone, usable), (r.clone.to_string_lossy().into_owned(), true, true)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_branch_checked_out_in_a_worktree_at_any_path_is_found() {
        let r = repos();
        let elsewhere = r.root.join("my-own-tree");
        sh(&r.clone, &["worktree", "add", "-q", "--track", "-b", "feature/PROJ-7-login", elsewhere.to_str().unwrap(), "origin/feature/PROJ-7-login"]);
        match resolve(&r, Some("feature/PROJ-7-login"), None).await {
            Workspace::Worktree { dir, is_main_clone, usable, .. } => assert_eq!((dir, is_main_clone, usable), (elsewhere.to_string_lossy().into_owned(), false, true)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dirty_or_mid_merge_checkout_is_reported_but_not_usable() {
        let r = repos();
        sh(&r.clone, &["checkout", "-q", "-b", "feature/PROJ-7-login", "--track", "origin/feature/PROJ-7-login"]);
        fs::write(r.clone.join("README.md"), "changed\n").unwrap();
        match resolve(&r, Some("feature/PROJ-7-login"), None).await {
            Workspace::Worktree { usable, state, reason, .. } => {
                assert!(!usable && state.dirty);
                assert!(reason.unwrap().contains("1 uncommitted change"));
            }
            other => panic!("{other:?}"),
        }
        sh(&r.clone, &["checkout", "--", "README.md"]);
        fs::write(r.clone.join(".git").join("MERGE_HEAD"), format!("{}\n", sh(&r.clone, &["rev-parse", "HEAD"]))).unwrap();
        match resolve(&r, Some("feature/PROJ-7-login"), None).await {
            Workspace::Worktree { usable, state, reason, .. } => {
                assert!(!usable);
                assert_eq!(state.in_progress, Some("merge"));
                assert!(reason.unwrap().contains("merge is in progress"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_branch_only_on_origin_is_offered_and_nothing_is_created() {
        let r = repos();
        let before = worktree_count(&r.clone);
        match resolve(&r, Some("feature/PROJ-7-login"), None).await {
            Workspace::Branch { remote, local, .. } => assert_eq!((remote, local), (true, false)),
            other => panic!("{other:?}"),
        }
        assert_eq!(worktree_count(&r.clone), before);
        assert!(!r.wt_root.exists());
    }

    #[tokio::test]
    async fn an_unknown_branch_is_none() {
        let r = repos();
        assert_eq!(resolve(&r, Some("feature/nope"), None).await, Workspace::None { branch: Some("feature/nope".into()) });
        assert_eq!(resolve(&r, Some("feature/nope"), None).await.to_json(), serde_json::json!({ "kind": "none", "branch": "feature/nope", "alternatives": [] }));
    }

    #[tokio::test]
    async fn with_only_a_ticket_key_it_prefers_the_branch_that_has_a_worktree_and_lists_the_others() {
        let r = repos();
        let tree = r.root.join("logout-tree");
        sh(&r.clone, &["worktree", "add", "-q", "--track", "-b", "feature/PROJ-7-logout", tree.to_str().unwrap(), "origin/feature/PROJ-7-logout"]);
        match resolve(&r, None, Some("PROJ-7")).await {
            Workspace::Worktree { branch, alternatives, .. } => assert_eq!((branch.as_str(), alternatives), ("feature/PROJ-7-logout", vec!["feature/PROJ-7-login".to_string()])),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn with_a_key_and_no_worktree_it_offers_a_branch_with_no_matching_branch_none() {
        let r = repos();
        match resolve(&r, None, Some("PROJ-7")).await {
            Workspace::Branch { alternatives, .. } => assert_eq!(alternatives.len(), 1),
            other => panic!("{other:?}"),
        }
        assert!(matches!(resolve(&r, None, Some("PROJ-99")).await, Workspace::None { .. }));
    }

    #[tokio::test]
    async fn resolve_workspace_refuses_an_unsafe_branch_a_bad_key_and_no_input() {
        let r = repos();
        assert!(resolve_workspace(&r.clone, Some("--upload-pack=x"), None, &SystemGit).await.unwrap_err().contains("safe branch"));
        assert!(resolve_workspace(&r.clone, None, Some("nope"), &SystemGit).await.unwrap_err().contains("issue key"));
        assert!(resolve_workspace(&r.clone, None, None, &SystemGit).await.unwrap_err().contains("branch or an issue key"));
    }

    #[tokio::test]
    async fn create_branch_worktree_makes_a_tracking_worktree_for_an_origin_branch_once() {
        let r = repos();
        let made = create_branch_worktree(&r.clone, "feature/PROJ-7-login", &r.home, &SystemGit).await.unwrap();
        assert_eq!(made.dir, r.wt_root.join("feature-PROJ-7-login"));
        assert_eq!(sh(&made.dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/PROJ-7-login");
        assert_eq!(sh(&made.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature/PROJ-7-login");
        match resolve(&r, Some("feature/PROJ-7-login"), None).await {
            Workspace::Worktree { dir, .. } => assert_eq!(dir, made.dir.to_string_lossy()),
            other => panic!("{other:?}"),
        }
        assert!(create_branch_worktree(&r.clone, "feature/PROJ-7-login", &r.home, &SystemGit).await.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn create_branch_worktree_never_invents_a_branch() {
        let r = repos();
        let e = create_branch_worktree(&r.clone, "feature/new-thing", &r.home, &SystemGit).await.unwrap_err();
        assert!(e.contains("fetch") || e.contains("doesn't exist"), "{e}");
        assert_eq!(worktree_count(&r.clone), 1);
        assert!(create_branch_worktree(&r.clone, "-x", &r.home, &SystemGit).await.is_err());
    }

    #[test]
    fn branch_worktree_names_replace_odd_characters_one_per_utf16_unit() {
        let p = branch_worktree_path(Path::new("/r/app"), "feature/a b.c_d-e", Path::new("/h"));
        assert_eq!(p.file_name().unwrap(), "feature-a-b.c_d-e");
        assert_eq!(branch_worktree_path(Path::new("/r/app"), "a😀b", Path::new("/h")).file_name().unwrap(), "a--b");
    }
}
