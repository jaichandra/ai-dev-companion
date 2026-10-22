//! Whether a directory is a linked worktree that git itself has registered for
//! a repo, read from the repo's own `.git/worktrees/<name>/gitdir` files (each
//! holds the path of that worktree's `.git` file); port of
//! `core/worktree-registry.js`. Synchronous and git-free so the job loader can
//! use it on untrusted job files at startup. The repo's own directory is not a
//! linked worktree, so it never counts.

use crate::js_text::js_trim;
use crate::node_path;
use std::fs;

/// `realpath`, or the lexically resolved path when that fails.
pub(crate) fn real(p: &str) -> String {
    fs::canonicalize(p)
        .ok()
        .and_then(|c| c.to_str().map(str::to_string))
        .unwrap_or_else(|| node_path::resolve(p))
}

pub fn is_linked_worktree_of(repo_path: &str, dir: &str) -> bool {
    if !node_path::is_absolute(dir) {
        return false;
    }
    let base = node_path::join(&[repo_path, ".git", "worktrees"]);
    let Ok(names) = fs::read_dir(&base) else { return false };
    let want = node_path::join(&[&real(dir), ".git"]);
    names.flatten().any(|entry| {
        // A half-removed registration (no readable gitdir file) is not a match.
        let Ok(text) = fs::read_to_string(entry.path().join("gitdir")) else { return false };
        let gitdir = js_trim(&text);
        !gitdir.is_empty() && node_path::join(&[&real(&node_path::dirname(gitdir)), ".git"]) == want
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(cwd).output().unwrap();
        assert!(st.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&st.stderr));
    }

    #[test]
    fn only_a_linked_worktree_git_registered_for_the_repo_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let repo = root.join("app");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "master"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "T"]);
        fs::write(repo.join("a"), "a").unwrap();
        git(&repo, &["add", "a"]);
        git(&repo, &["commit", "-q", "-m", "i"]);
        let tree = root.join("elsewhere").join("tree");
        git(&repo, &["worktree", "add", "-q", "-b", "feat", &s(&tree)]);

        assert!(is_linked_worktree_of(&s(&repo), &s(&tree)));
        assert!(!is_linked_worktree_of(&s(&repo), &s(&repo)), "the clone itself is not a linked worktree");
        assert!(!is_linked_worktree_of(&s(&repo), &s(&root.join("elsewhere"))));
        assert!(!is_linked_worktree_of(&s(&repo), "relative/tree"));
        assert!(!is_linked_worktree_of(&s(&root.join("no-repo")), &s(&tree)));
        fs::remove_dir_all(&tree).unwrap();
        // Removed outside git: the registration is still there, but the folder is gone.
        assert!(is_linked_worktree_of(&s(&repo), &s(&tree)));
    }
}
