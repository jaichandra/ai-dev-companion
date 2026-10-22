//! Defense in depth against a resolve-conflict Claude run quietly redirecting
//! how git behaves the NEXT time the service itself runs git in this worktree
//! (port of `core/worktree-integrity.js`), by rewriting the worktree's own
//! `.git` gitlink file, or the metadata git keeps for this worktree in the
//! MAIN repo (`<gitDir>/commondir`, `<gitDir>/gitdir`,
//! `<gitDir>/config.worktree`), to point git at an attacker-controlled config
//! (`core.hooksPath`, `core.fsmonitor`, ...). The edit guard and the sandbox's
//! write denial are the primary defense; this is the fallback: snapshot these
//! four paths right before Claude runs, and refuse to run ANY further git
//! command in the worktree if they changed.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMetadataSnapshot {
    pub gitlink: Option<String>,
    pub commondir: Option<String>,
    pub gitdir: Option<String>,
    pub config_worktree: Option<String>,
}

/// The file as text, or `None` if it doesn't exist (e.g. `config.worktree`,
/// which most worktrees never have: a normal, comparable snapshot value). Any
/// other read failure (permissions, the path being a directory) is NOT
/// swallowed: this check exists to notice something unexpected, so treating an
/// unreadable file as "unchanged" would defeat the point.
fn read_if_exists(p: &Path) -> io::Result<Option<String>> {
    match fs::read(p) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `worktree_dir` is the worktree's own checkout (its `.git` is a one-line
/// gitlink FILE); `git_dir` is `git rev-parse --absolute-git-dir` run inside
/// it, the worktree's private metadata dir under the main repo.
pub fn snapshot_worktree_git_metadata(worktree_dir: &Path, git_dir: &Path) -> io::Result<GitMetadataSnapshot> {
    Ok(GitMetadataSnapshot {
        gitlink: read_if_exists(&worktree_dir.join(".git"))?,
        commondir: read_if_exists(&git_dir.join("commondir"))?,
        gitdir: read_if_exists(&git_dir.join("gitdir"))?,
        config_worktree: read_if_exists(&git_dir.join("config.worktree"))?,
    })
}

/// True if anything differs, including a file appearing or disappearing.
pub fn worktree_git_metadata_changed(before: &GitMetadataSnapshot, after: &GitMetadataSnapshot) -> bool {
    before != after
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture {
        _root: tempfile::TempDir,
        worktree_dir: PathBuf,
        git_dir: PathBuf,
    }

    /// Laid out like a real worktree's private metadata: `<tmp>/worktree/.git`
    /// (the gitlink file) and `<tmp>/gitdir/{commondir,gitdir}` (config.worktree
    /// deliberately absent: most worktrees never have one).
    fn make_fixture() -> Fixture {
        let root = tempfile::Builder::new().prefix("worktree-integrity-test-").tempdir().unwrap();
        let worktree_dir = root.path().join("worktree");
        let git_dir = root.path().join("gitdir");
        fs::create_dir_all(&worktree_dir).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(worktree_dir.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        fs::write(git_dir.join("gitdir"), format!("{}/.git\n", worktree_dir.display())).unwrap();
        Fixture { _root: root, worktree_dir, git_dir }
    }

    fn snap(f: &Fixture) -> GitMetadataSnapshot {
        snapshot_worktree_git_metadata(&f.worktree_dir, &f.git_dir).unwrap()
    }

    #[test]
    fn snapshot_reads_the_gitlink_commondir_gitdir_and_none_for_a_missing_config_worktree() {
        let f = make_fixture();
        let s = snap(&f);
        assert_eq!(s.gitlink, Some(format!("gitdir: {}\n", f.git_dir.display())));
        assert_eq!(s.commondir.as_deref(), Some("../..\n"));
        assert_eq!(s.gitdir, Some(format!("{}/.git\n", f.worktree_dir.display())));
        assert_eq!(s.config_worktree, None);
    }

    #[test]
    fn untouched_metadata_is_unchanged() {
        let f = make_fixture();
        assert!(!worktree_git_metadata_changed(&snap(&f), &snap(&f)));
    }

    #[test]
    fn each_of_the_four_files_changing_or_appearing_is_a_change() {
        let cases: [(&str, &str, bool); 4] = [
            ("worktree/.git", "gitdir: /somewhere/else\n", false),
            ("gitdir/commondir", "/attacker/controlled\n", false),
            ("gitdir/gitdir", "/attacker/controlled/.git\n", false),
            ("gitdir/config.worktree", "[core]\n\thooksPath = /tmp/evil\n", true),
        ];
        for (rel, content, _appears) in cases {
            let f = make_fixture();
            let before = snap(&f);
            fs::write(f._root.path().join(rel), content).unwrap();
            assert!(worktree_git_metadata_changed(&before, &snap(&f)), "{rel}");
        }
    }

    #[test]
    fn a_file_disappearing_is_a_change_and_an_unreadable_one_is_an_error_not_unchanged() {
        let f = make_fixture();
        let before = snap(&f);
        fs::remove_file(f.git_dir.join("commondir")).unwrap();
        assert!(worktree_git_metadata_changed(&before, &snap(&f)));

        // `.git` being a directory (a read error that isn't ENOENT) must not pass silently.
        let f = make_fixture();
        fs::remove_file(f.worktree_dir.join(".git")).unwrap();
        fs::create_dir(f.worktree_dir.join(".git")).unwrap();
        assert!(snapshot_worktree_git_metadata(&f.worktree_dir, &f.git_dir).is_err());
    }
}
