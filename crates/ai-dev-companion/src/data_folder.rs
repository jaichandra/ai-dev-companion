//! The state folder (`~/.ai-dev-companion`): how much space it uses and
//! clearing the part that can be rebuilt (port of `core/data-folder.js`).
//!
//! Only checkouts are purgeable: `worktrees/` (job worktrees), `repos/` (the
//! analysis clones) and every `<repo>.worktrees/` (the persistent review
//! worktrees). Everything else (history.db, jobs/, sessions/, credentials,
//! scheduler and notification state) is never touched.

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn is_purgeable(name: &str) -> bool {
    name == "worktrees" || name == "repos" || name.ends_with(".worktrees")
}

/// Bytes under `p`. lstat throughout, so the `node_modules` symlinks inside a
/// worktree are counted as links, never followed into the real clone.
pub fn size_of(p: &Path) -> u64 {
    let Ok(st) = fs::symlink_metadata(p) else { return 0 };
    if !st.is_dir() {
        return st.len();
    }
    let Ok(entries) = fs::read_dir(p) else { return st.len() };
    st.len() + entries.flatten().map(|e| size_of(&e.path())).sum::<u64>()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub name: String,
    pub bytes: u64,
    pub purgeable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub path: String,
    pub total_bytes: u64,
    pub purgeable_bytes: u64,
    pub items: Vec<Item>,
}

fn names_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

pub fn data_folder_usage(dir: &Path) -> Usage {
    let mut items: Vec<Item> = names_in(dir)
        .into_iter()
        .map(|name| Item { bytes: size_of(&dir.join(&name)), purgeable: is_purgeable(&name), name })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.bytes));
    Usage {
        path: dir.to_string_lossy().into_owned(),
        total_bytes: items.iter().map(|i| i.bytes).sum(),
        purgeable_bytes: items.iter().filter(|i| i.purgeable).map(|i| i.bytes).sum(),
        items,
    }
}

/// The clone a worktree belongs to, read from its `.git` file
/// (`gitdir: <clone>/.git/worktrees/<id>`); `None` when it can't be told.
pub fn owning_clone(worktree_dir: &Path) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(&fs::read(worktree_dir.join(".git")).ok()?).into_owned();
    let line = text.split(['\n', '\r']).find_map(|l| l.strip_prefix("gitdir:"))?;
    let gitdir = crate::js_text::js_trim(line);
    if gitdir.is_empty() {
        return None;
    }
    let at = gitdir.find("/.git/worktrees/")?;
    (at > 0).then(|| PathBuf::from(&gitdir[..at]))
}

fn worktree_dirs_under(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let child = e.path();
            // `stat` follows links, like the JS `statSync(...).isFile()`.
            if fs::metadata(child.join(".git")).is_ok_and(|m| m.is_file()) {
                found.push(child);
                continue;
            }
            if depth < 2 {
                walk(&child, depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, 0, &mut found);
    found
}

fn git_worktree_prune(clone: &Path) {
    let _ = Command::new("git").args(["worktree", "prune"]).current_dir(clone).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Purged {
    pub removed: Vec<String>,
    pub freed_bytes: u64,
}

/// Deletes the purgeable folders, then asks each owning clone to forget the
/// worktrees that are gone. Returns what was removed.
pub fn purge_data_folder(dir: &Path) -> Purged {
    let names = names_in(dir);
    let targets: Vec<&String> = names.iter().filter(|n| is_purgeable(n)).collect();
    let mut clones: Vec<PathBuf> = Vec::new();
    for name in &targets {
        for wt in worktree_dirs_under(&dir.join(name)) {
            if let Some(c) = owning_clone(&wt) {
                if !clones.contains(&c) {
                    clones.push(c);
                }
            }
        }
    }
    let mut removed = Vec::new();
    let mut freed_bytes = 0;
    for name in targets {
        let target = dir.join(name);
        freed_bytes += size_of(&target);
        // A purgeable name that is itself a link: remove the link, never what it points at.
        let is_link = fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink());
        let _ = if is_link { fs::remove_file(&target) } else { fs::remove_dir_all(&target) };
        removed.push(name.clone());
    }
    for clone in clones.iter().filter(|c| c.exists()) {
        git_worktree_prune(clone);
    }
    Purged { removed, freed_bytes }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::Builder::new().prefix("data-folder-").tempdir().unwrap();
        let p = d.path();
        fs::create_dir_all(p.join("worktrees/repo/job1")).unwrap();
        fs::write(p.join("worktrees/repo/job1/a.txt"), "x".repeat(1000)).unwrap();
        fs::create_dir_all(p.join("repo.worktrees/pr-review")).unwrap();
        fs::write(p.join("repo.worktrees/pr-review/b.txt"), "y".repeat(500)).unwrap();
        fs::create_dir(p.join("jobs")).unwrap();
        fs::write(p.join("jobs/j.json"), "{}").unwrap();
        fs::write(p.join("history.db"), "z".repeat(200)).unwrap();
        d
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn only_checkouts_are_purgeable() {
        for ok in ["worktrees", "repos", "sample-app.worktrees"] {
            assert!(is_purgeable(ok), "{ok}");
        }
        for keep in ["jobs", "history.db", "sessions", "credentials.key", "worktrees.json"] {
            assert!(!is_purgeable(keep), "{keep}");
        }
    }

    #[test]
    fn usage_totals_the_folder_and_splits_out_what_a_purge_frees() {
        let d = fixture();
        let u = data_folder_usage(d.path());
        assert!(u.total_bytes >= 1700);
        assert!(u.purgeable_bytes >= 1500 && u.purgeable_bytes < u.total_bytes);
        let purgeable: Vec<String> = u.items.iter().filter(|i| i.purgeable).map(|i| i.name.clone()).collect();
        assert_eq!(sorted(purgeable), ["repo.worktrees", "worktrees"]);
        assert!(u.items.windows(2).all(|w| w[0].bytes >= w[1].bytes), "largest first");
        let json = serde_json::to_value(&u).unwrap();
        assert!(json["totalBytes"].is_number() && json["purgeableBytes"].is_number());
    }

    #[test]
    fn a_missing_folder_reports_zero() {
        let u = data_folder_usage(Path::new("/does-not-exist-data-folder-xyz"));
        assert_eq!((u.total_bytes, u.items.len()), (0, 0));
    }

    #[test]
    fn purge_removes_checkouts_and_keeps_history_jobs_and_the_rest() {
        let d = fixture();
        let r = purge_data_folder(d.path());
        assert_eq!(sorted(r.removed), ["repo.worktrees", "worktrees"]);
        assert!(r.freed_bytes >= 1500);
        assert_eq!(sorted(names_in(d.path())), ["history.db", "jobs"]);
    }

    #[test]
    fn a_symlink_inside_a_worktree_is_not_followed() {
        let d = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep.txt"), "k".repeat(5000)).unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("worktrees/repo/job1/node_modules")).unwrap();
        assert!(data_folder_usage(d.path()).total_bytes < 5000);
        purge_data_folder(d.path());
        assert!(outside.path().join("keep.txt").exists());
    }

    #[test]
    fn a_purgeable_name_that_is_a_link_loses_only_the_link() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep.txt"), "k").unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("repos")).unwrap();
        let r = purge_data_folder(d.path());
        assert_eq!(r.removed, ["repos"]);
        assert!(outside.path().join("keep.txt").exists());
        assert!(!d.path().join("repos").exists());
    }

    #[test]
    fn owning_clone_reads_the_clone_from_a_worktrees_git_file() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".git"), "gitdir: /x/clone/.git/worktrees/job1\n").unwrap();
        assert_eq!(owning_clone(d.path()), Some(PathBuf::from("/x/clone")));
        assert_eq!(owning_clone(&d.path().join("nope")), None);
        fs::write(d.path().join(".git"), "junk\n").unwrap();
        assert_eq!(owning_clone(d.path()), None);
        fs::write(d.path().join(".git"), "gitdir: /not/a/worktree\n").unwrap();
        assert_eq!(owning_clone(d.path()), None);
        fs::write(d.path().join(".git"), "gitdir: /.git/worktrees/x\n").unwrap();
        assert_eq!(owning_clone(d.path()), None, "an empty clone path is not a clone");
    }

    #[test]
    fn purging_asks_the_owning_clone_to_forget_the_removed_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let o = Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        let clone = root.path().join("clone");
        fs::create_dir(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "master"]);
        git(&clone, &["config", "user.email", "t@example.com"]);
        git(&clone, &["config", "user.name", "T"]);
        fs::write(clone.join("a"), "a").unwrap();
        git(&clone, &["add", "a"]);
        git(&clone, &["commit", "-q", "-m", "i"]);
        let data = root.path().join("data");
        fs::create_dir_all(data.join("worktrees")).unwrap();
        git(&clone, &["worktree", "add", "-q", "-b", "job", data.join("worktrees/job1").to_str().unwrap()]);
        let listed = |c: &Path| String::from_utf8(Command::new("git").args(["worktree", "list"]).current_dir(c).output().unwrap().stdout).unwrap();
        assert!(listed(&clone).contains("job1"));
        purge_data_folder(&data);
        assert!(!listed(&clone).contains("job1"), "pruned: {}", listed(&clone));
    }
}
