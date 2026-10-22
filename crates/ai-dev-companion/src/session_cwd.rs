//! Whether a persisted job's recorded Claude-session directory is somewhere
//! this service itself would have run Claude for a job with no worktree (port
//! of `core/session-cwd.js`): a mapped repo clone, one of analyze-issue's
//! read-only checkouts (`<stateDir>/repos/<PROJECT>/<repo>`), a folder inside a
//! mapped repo's `<stateDir>/<repo>.worktrees/`, a per-job worktree
//! (`<worktreesRoot>/<repo>/<id>`), or a per-ticket directory under
//! `<stateDir>/sessions`. A job file is untrusted input, and "Continue in
//! Claude Code" opens a terminal in this directory: a hand-edited file must
//! not be able to point it at an attacker's folder (whose `.claude` settings
//! would then load).

use crate::job_files::is_strictly_inside;
use crate::node_path;
use crate::paths::repo_worktrees_root;
use crate::worktree_registry::is_linked_worktree_of;
use std::path::Path;

pub fn strictly_inside(root: Option<&str>, target: Option<&str>) -> bool {
    is_strictly_inside(root, target)
}

/// `<repo>.worktrees/<one folder>`: exactly one level down, so the
/// `.worktrees` folder itself (or anything deeper) never counts.
fn is_repo_worktree_dir(repo_path: &str, resolved: &str, home: &Path) -> bool {
    let root = repo_worktrees_root(Path::new(repo_path), home).to_string_lossy().into_owned();
    strictly_inside(Some(&root), Some(resolved)) && node_path::dirname(resolved) == root
}

#[derive(Default)]
pub struct SessionCwdRules<'a> {
    pub repo_paths: &'a [String],
    pub sessions_root: Option<&'a str>,
    pub checkouts_root: Option<&'a str>,
    pub worktrees_root: Option<&'a str>,
}

/// A path exactly two levels under `root`.
fn two_levels_under(root: Option<&str>, resolved: &str) -> bool {
    let Some(root) = root.filter(|r| strictly_inside(Some(r), Some(resolved))) else { return false };
    let prefix = format!("{}/", node_path::resolve(root).trim_end_matches('/'));
    resolved.strip_prefix(&prefix).is_some_and(|rel| rel.split('/').count() == 2)
}

pub fn session_cwd_allowed(cwd: Option<&str>, rules: &SessionCwdRules<'_>, home: &Path) -> bool {
    let Some(cwd) = cwd.filter(|c| node_path::is_absolute(c)) else { return false };
    let resolved = node_path::resolve(cwd);
    if rules.repo_paths.iter().any(|p| node_path::resolve(p) == resolved) {
        return true;
    }
    if rules.repo_paths.iter().any(|p| is_repo_worktree_dir(p, &resolved, home)) {
        return true;
    }
    // A worktree of yours that a job or Continue in Claude Code reused: anywhere,
    // as long as git has registered it as a linked worktree of a mapped repo.
    if rules.repo_paths.iter().any(|p| is_linked_worktree_of(p, &resolved)) {
        return true;
    }
    // The read-only checkouts: <checkoutsRoot>/<PROJECT>/<repo>; and the per-job worktrees: <worktreesRoot>/<repo>/<id>.
    if two_levels_under(rules.checkouts_root, &resolved) || two_levels_under(rules.worktrees_root, &resolved) {
        return true;
    }
    strictly_inside(rules.sessions_root, Some(&resolved))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git_cli::testing::git;
    use std::path::PathBuf;

    fn home() -> PathBuf {
        PathBuf::from("/Users/me")
    }

    fn wt() -> String {
        repo_worktrees_root(Path::new("/Users/me/gitviews/sample-app"), &home()).to_string_lossy().into_owned()
    }

    fn repos() -> Vec<String> {
        vec!["/Users/me/gitviews/sample-app".to_string()]
    }

    fn allowed(cwd: Option<&str>, rules: &SessionCwdRules<'_>) -> bool {
        session_cwd_allowed(cwd, rules, &home())
    }

    #[test]
    fn a_mapped_clone_a_sessions_folder_and_a_repo_worktrees_key_folder_are_allowed() {
        let r = repos();
        let rules = SessionCwdRules { repo_paths: &r, sessions_root: Some("/Users/me/.ai-dev-companion/sessions"), ..Default::default() };
        assert!(allowed(Some("/Users/me/gitviews/sample-app"), &rules));
        assert!(allowed(Some("/Users/me/.ai-dev-companion/sessions/PROJ-7"), &rules));
        assert!(allowed(Some(&format!("{}/PROJ-7", wt())), &rules));
    }

    #[test]
    fn anything_else_is_refused() {
        let r = repos();
        let rules = SessionCwdRules { repo_paths: &r, sessions_root: Some("/Users/me/.ai-dev-companion/sessions"), ..Default::default() };
        for cwd in [
            wt(),
            format!("{}/PROJ-7/src", wt()),
            format!("{}/../evil", wt()),
            "/Users/me/.ai-dev-companion/other.worktrees/PROJ-7".to_string(),
            "/tmp/evil".to_string(),
            "sample-app.worktrees/PROJ-7".to_string(),
            "/Users/me/.ai-dev-companion/sessions".to_string(),
        ] {
            assert!(!allowed(Some(&cwd), &rules), "{cwd}");
        }
        assert!(!allowed(None, &rules));
    }

    #[test]
    fn a_read_only_checkout_is_allowed_exactly_two_levels_under_the_checkouts_root() {
        let rules = SessionCwdRules { sessions_root: Some("/Users/me/.ai-dev-companion/sessions"), checkouts_root: Some("/Users/me/.ai-dev-companion/repos"), ..Default::default() };
        assert!(allowed(Some("/Users/me/.ai-dev-companion/repos/ACME/sample-app"), &rules));
        for no in ["/Users/me/.ai-dev-companion/repos/ACME", "/Users/me/.ai-dev-companion/repos/ACME/sample-app/src", "/Users/me/.ai-dev-companion/repos"] {
            assert!(!allowed(Some(no), &rules), "{no}");
        }
        let none = SessionCwdRules { checkouts_root: None, ..SessionCwdRules { sessions_root: rules.sessions_root, ..Default::default() } };
        assert!(!allowed(Some("/Users/me/.ai-dev-companion/repos/ACME/sample-app"), &none));
    }

    #[test]
    fn a_per_job_worktree_is_two_levels_under_the_worktrees_root() {
        let rules = SessionCwdRules { worktrees_root: Some("/s/worktrees"), ..Default::default() };
        assert!(allowed(Some("/s/worktrees/app/11111111-1111-1111-1111-111111111111"), &rules));
        assert!(!allowed(Some("/s/worktrees/app"), &rules));
        assert!(!allowed(Some("/s/worktrees/app/x/y"), &rules));
    }

    #[test]
    fn a_linked_worktree_git_registered_for_a_mapped_repo_is_allowed_wherever_it_is_a_lookalike_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let repo = root.join("app");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "master"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("a"), "a").unwrap();
        git(&repo, &["add", "a"]);
        git(&repo, &["commit", "-q", "-m", "i"]);
        let mine = root.join("my-own-tree");
        git(&repo, &["worktree", "add", "-q", "-b", "feat", mine.to_str().unwrap()]);
        let paths = vec![repo.to_string_lossy().into_owned()];
        let sessions = root.join("sessions").to_string_lossy().into_owned();
        let rules = SessionCwdRules { repo_paths: &paths, sessions_root: Some(&sessions), ..Default::default() };
        assert!(allowed(Some(mine.to_str().unwrap()), &rules));
        assert!(!allowed(Some(root.join("not-a-worktree").to_str().unwrap()), &rules));
        let other = vec![root.join("other").to_string_lossy().into_owned()];
        assert!(!allowed(Some(mine.to_str().unwrap()), &SessionCwdRules { repo_paths: &other, ..rules }));
    }
}
