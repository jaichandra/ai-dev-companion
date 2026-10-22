//! The single source of truth for where this service's own state lives on disk,
//! and the one-time move off the old (bitbucket-ai-companion-branded) directory
//! name onto this one (port of `core/paths.js`). Every other module gets the
//! state dir from here rather than building `~/.something` paths by hand, so
//! there is exactly one place that knows both names.

use crate::app_identity::app_slug;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LEGACY_STATE_DIR_NAME: &str = ".bitbucket-ai-companion";

/// `.<app slug>`.
pub fn state_dir_name() -> String {
    format!(".{}", app_slug())
}

/// The user's home directory: `$HOME`, else the passwd entry.
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    #[cfg(unix)]
    {
        let mut buf = vec![0 as libc::c_char; 4096];
        // SAFETY: a zeroed passwd is a valid out-parameter for getpwuid_r, which
        // fills it (and points `pw_dir` into `buf`) on success.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) };
        if rc == 0 && !out.is_null() && !pwd.pw_dir.is_null() {
            // SAFETY: pw_dir is a NUL-terminated string inside `buf`.
            let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
            return PathBuf::from(dir.to_string_lossy().into_owned());
        }
    }
    PathBuf::from("/")
}

pub fn state_dir(home: &Path) -> PathBuf {
    home.join(state_dir_name())
}

/// `<stateDir>/<repo>.worktrees`: where every feature keeps a repo's persistent
/// worktrees (pr-review, `<KEY>`), rather than beside the clone.
pub fn repo_worktrees_root(repo_path: &Path, home: &Path) -> PathBuf {
    let base = repo_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    state_dir(home).join(format!("{base}.worktrees"))
}

pub fn legacy_state_dir(home: &Path) -> PathBuf {
    home.join(LEGACY_STATE_DIR_NAME)
}

/// What a failed git run reports: its message, and git's own complaint if the
/// runner captured one (worth surfacing: the message alone is often just
/// "Command failed: git ...").
#[derive(Debug, Clone)]
pub struct GitFailure {
    pub message: String,
    pub stderr: Option<String>,
}

pub type GitRunner<'a> = &'a dyn Fn(&[String], &Path) -> Result<(), GitFailure>;

/// Runs `git` for the worktree repairs. Synchronous, so the one-time startup
/// migration can stay a plain statement before the server is built.
pub fn default_git(args: &[String], cwd: &Path) -> Result<(), GitFailure> {
    let out = Command::new("git").args(args).current_dir(cwd).output().map_err(|e| GitFailure { message: e.to_string(), stderr: None })?;
    if out.status.success() {
        return Ok(());
    }
    Err(GitFailure { message: format!("Command failed: git {}", args.join(" ")), stderr: Some(String::from_utf8_lossy(&out.stderr).into_owned()) })
}

fn copy_dir_all(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// `rename`, falling back to a recursive copy-then-delete when the two paths
/// are on different filesystems and a plain rename can't cross them.
fn rename_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(src, dest) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_dir_all(src, dest)?;
            fs::remove_dir_all(src)
        }
        other => other,
    }
}

/// Whether `p` names a live, followable directory: false, and never failing,
/// for anything else, including a symlink whose target no longer exists. A
/// broken entry left under `worktrees/` (by some external cleanup, a
/// half-finished job) is just skipped rather than aborting the whole scan.
fn is_live_directory(p: &Path) -> bool {
    match fs::symlink_metadata(p) {
        Err(_) => false,
        Ok(m) if !m.file_type().is_symlink() => m.is_dir(),
        Ok(_) => fs::metadata(p).is_ok_and(|m| m.is_dir()),
    }
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// The checkout folders directly inside `parent` that are git worktrees (a
/// `.git` *file*, not a clone's directory). Skips broken entries.
fn worktree_children(parent: &Path) -> Vec<PathBuf> {
    if !is_live_directory(parent) {
        return vec![];
    }
    sorted_children(parent).into_iter().filter(|d| is_live_directory(d) && fs::metadata(d.join(".git")).is_ok_and(|m| m.is_file())).collect()
}

/// Every git worktree checkout the service keeps under `dir`: the disposable
/// per-job ones at `<dir>/worktrees/<repo>/<id>/` and the persistent ones at
/// `<dir>/<repo>.worktrees/<name>/`. Identified by a `.git` *file* there (a
/// real clone has a `.git` directory; a worktree's is a one-line file pointing
/// back at the main repo). A broken entry is skipped, not fatal to the scan.
pub fn find_worktree_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let root = dir.join("worktrees");
    if root.exists() {
        for repo in sorted_children(&root) {
            found.extend(worktree_children(&repo));
        }
    }
    if is_live_directory(dir) {
        for child in sorted_children(dir) {
            if child.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".worktrees")) {
                found.extend(worktree_children(&child));
            }
        }
    }
    found
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Migration {
    pub migrated: bool,
    pub repaired: Vec<PathBuf>,
    pub errors: Vec<String>,
}

/// One-time move from the old `~/.bitbucket-ai-companion` to the new
/// `~/.<slug>`, meant to run once at server startup before anything else
/// touches either path. A no-op unless the legacy dir exists and the new one
/// doesn't, so a second run, or an install that never had a legacy dir, does
/// nothing.
///
/// Moving the directory does NOT touch a worktree checkout's own `.git` file
/// (it points back at its main repo, which never moves). What DOES go stale is
/// the *main repo's* record of where the worktree lives
/// (`<main-repo>/.git/worktrees/<id>/gitdir`), which still names the old
/// location. `git worktree repair`, run from inside each moved worktree,
/// rewrites it. A repair failure is logged and ignored rather than aborting:
/// a broken worktree link is recoverable by hand and shouldn't strand the rest
/// of the state directory mid-move.
///
/// Never fails: any unexpected problem (including the move itself) lands in
/// `errors`, so a broken migration can never keep the service from starting.
pub fn migrate_state_dir(home: &Path, git: GitRunner<'_>, log: &dyn Fn(&str)) -> Migration {
    let (legacy, current) = (legacy_state_dir(home), state_dir(home));
    let mut result = Migration::default();
    if !legacy.exists() || current.exists() {
        return result;
    }
    if let Err(e) = rename_or_copy(&legacy, &current) {
        result.errors.push(format!("Could not move {} to {}: {e}", legacy.display(), current.display()));
        return result;
    }
    result.migrated = true;

    for worktree in find_worktree_dirs(&current) {
        let args = vec!["worktree".to_string(), "repair".to_string(), worktree.to_string_lossy().into_owned()];
        match git(&args, &worktree) {
            Ok(()) => result.repaired.push(worktree),
            Err(e) => {
                let detail = e.stderr.as_deref().map(str::trim).filter(|s| !s.is_empty()).map_or(e.message.clone(), str::to_string);
                let message = format!("git worktree repair failed for {}: {detail}", worktree.display());
                log(&message);
                result.errors.push(message);
            }
        }
    }

    let marker = legacy.join("MOVED");
    if let Err(e) = fs::create_dir_all(&legacy).and_then(|()| fs::write(&marker, format!("{}\n", current.display()))) {
        result.errors.push(format!("Could not write {}: {e}", marker.display()));
    }
    result
}

/// Whether [`migrate_state_dir`] should even run: not for a process that isn't
/// running from the stable install location (a dev checkout, say), and never
/// when the operator opted out with `AI_DEV_COMPANION_SKIP_MIGRATION=1`: both
/// keep a dev checkout from moving a *live* install's state out from under it.
pub fn should_migrate(is_stable_install: bool, skip_migration_env: Option<&str>) -> bool {
    skip_migration_env != Some("1") && is_stable_install
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn fake_worktree(home: &Path, dir_name: &str, repo: &str, id: &str) -> PathBuf {
        let dir = home.join(dir_name).join("worktrees").join(repo).join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        dir
    }

    fn ok_git() -> impl Fn(&[String], &Path) -> Result<(), GitFailure> {
        |_, _| Ok(())
    }

    #[test]
    fn state_dir_and_legacy_state_dir_join_the_names_onto_home() {
        assert_eq!(state_dir(Path::new("/h")), PathBuf::from("/h/.ai-dev-companion"));
        assert_eq!(legacy_state_dir(Path::new("/h")), PathBuf::from("/h/.bitbucket-ai-companion"));
        assert_eq!(state_dir_name(), ".ai-dev-companion");
        assert_eq!(LEGACY_STATE_DIR_NAME, ".bitbucket-ai-companion");
        assert_eq!(repo_worktrees_root(Path::new("/code/sample-app"), Path::new("/h")), PathBuf::from("/h/.ai-dev-companion/sample-app.worktrees"));
        assert!(home_dir().is_absolute());
    }

    #[test]
    fn migrate_moves_the_legacy_dir_to_the_new_one_and_reports_migrated() {
        let home = tempfile::tempdir().unwrap();
        let legacy = legacy_state_dir(home.path());
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("config.json"), "{}").unwrap();
        let r = migrate_state_dir(home.path(), &ok_git(), &|_| {});
        assert!(r.migrated && r.errors.is_empty());
        assert!(state_dir(home.path()).join("config.json").exists());
    }

    #[test]
    fn migrate_no_ops_when_only_the_new_dir_exists_when_neither_exists_and_when_both_do() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(migrate_state_dir(home.path(), &ok_git(), &|_| {}), Migration::default());
        fs::create_dir_all(state_dir(home.path())).unwrap();
        assert_eq!(migrate_state_dir(home.path(), &ok_git(), &|_| {}), Migration::default());
        assert!(!legacy_state_dir(home.path()).exists());
        fs::create_dir_all(legacy_state_dir(home.path())).unwrap();
        fs::write(legacy_state_dir(home.path()).join("legacy-marker"), "").unwrap();
        fs::write(state_dir(home.path()).join("new-marker"), "").unwrap();
        let r = migrate_state_dir(home.path(), &ok_git(), &|_| {});
        assert!(!r.migrated);
        assert!(legacy_state_dir(home.path()).join("legacy-marker").exists() && state_dir(home.path()).join("new-marker").exists());
    }

    #[test]
    fn migrate_repairs_every_moved_worktree_with_the_injected_git_runner() {
        let home = tempfile::tempdir().unwrap();
        fake_worktree(home.path(), ".bitbucket-ai-companion", "some-repo", "job-1");
        let calls = RefCell::new(Vec::new());
        let git = |args: &[String], cwd: &Path| {
            calls.borrow_mut().push((args.to_vec(), cwd.to_path_buf()));
            Ok(())
        };
        let r = migrate_state_dir(home.path(), &git, &|_| {});
        let new_path = state_dir(home.path()).join("worktrees/some-repo/job-1");
        assert_eq!(*calls.borrow(), [(vec!["worktree".to_string(), "repair".into(), new_path.to_string_lossy().into_owned()], new_path.clone())]);
        assert_eq!(r.repaired, [new_path]);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn migrate_also_repairs_the_persistent_repo_worktrees_name_worktrees() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(".bitbucket-ai-companion/some-repo.worktrees/PROJ-1");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        let calls = RefCell::new(Vec::new());
        let git = |args: &[String], cwd: &Path| {
            calls.borrow_mut().push((args.to_vec(), cwd.to_path_buf()));
            Ok(())
        };
        let r = migrate_state_dir(home.path(), &git, &|_| {});
        let moved = state_dir(home.path()).join("some-repo.worktrees/PROJ-1");
        assert_eq!(calls.borrow()[0].1, moved);
        assert_eq!(r.repaired, [moved]);
    }

    #[test]
    fn migrate_writes_a_moved_marker_pointing_at_the_new_location() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(legacy_state_dir(home.path())).unwrap();
        migrate_state_dir(home.path(), &ok_git(), &|_| {});
        let legacy = legacy_state_dir(home.path());
        let names: Vec<_> = fs::read_dir(&legacy).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["MOVED"]);
        assert_eq!(fs::read_to_string(legacy.join("MOVED")).unwrap().trim(), state_dir(home.path()).to_string_lossy());
    }

    #[test]
    fn migrate_never_fails_when_git_fails_and_records_it_and_prefers_stderr_over_the_message() {
        let home = tempfile::tempdir().unwrap();
        fake_worktree(home.path(), ".bitbucket-ai-companion", "some-repo", "job-1");
        let logged = RefCell::new(Vec::new());
        let boom = |_: &[String], _: &Path| Err(GitFailure { message: "git not found".into(), stderr: None });
        let r = migrate_state_dir(home.path(), &boom, &|m| logged.borrow_mut().push(m.to_string()));
        assert!(r.migrated && r.repaired.is_empty());
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].contains("git not found"));
        assert_eq!(logged.borrow().len(), 1);

        let home = tempfile::tempdir().unwrap();
        fake_worktree(home.path(), ".bitbucket-ai-companion", "some-repo", "job-1");
        let with_stderr = |_: &[String], _: &Path| Err(GitFailure { message: "Command failed: git worktree repair".into(), stderr: Some("fatal: not a git repository\n".into()) });
        let r = migrate_state_dir(home.path(), &with_stderr, &|_| {});
        assert!(r.errors[0].contains("fatal: not a git repository") && !r.errors[0].contains("Command failed"));
    }

    #[test]
    fn migrate_skips_a_dangling_symlink_under_worktrees_and_still_repairs_the_real_ones() {
        let home = tempfile::tempdir().unwrap();
        fake_worktree(home.path(), ".bitbucket-ai-companion", "some-repo", "job-1");
        let root = home.path().join(".bitbucket-ai-companion/worktrees");
        std::os::unix::fs::symlink(root.join("does-not-exist"), root.join("broken-repo-link")).unwrap();
        std::os::unix::fs::symlink(root.join("some-repo/does-not-exist"), root.join("some-repo/broken-id-link")).unwrap();
        let calls = RefCell::new(0);
        let git = |_: &[String], _: &Path| {
            *calls.borrow_mut() += 1;
            Ok(())
        };
        let r = migrate_state_dir(home.path(), &git, &|_| {});
        assert_eq!((r.repaired, r.errors.is_empty(), *calls.borrow()), (vec![state_dir(home.path()).join("worktrees/some-repo/job-1")], true, 1));
    }

    #[test]
    fn a_real_git_repair_runs_for_a_real_worktree() {
        let home = tempfile::tempdir().unwrap();
        let clone = home.path().join("clone");
        fs::create_dir_all(&clone).unwrap();
        let g = |args: &[&str], cwd: &Path| {
            let o = Command::new("git").args(args).current_dir(cwd).output().unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q", "-b", "master"], &clone);
        g(&["config", "user.email", "t@example.com"], &clone);
        g(&["config", "user.name", "T"], &clone);
        fs::write(clone.join("a"), "a").unwrap();
        g(&["add", "a"], &clone);
        g(&["commit", "-q", "-m", "i"], &clone);
        let wt = home.path().join(".bitbucket-ai-companion/worktrees/clone/job-1");
        fs::create_dir_all(wt.parent().unwrap()).unwrap();
        g(&["worktree", "add", "-q", "-b", "job", wt.to_str().unwrap()], &clone);
        let r = migrate_state_dir(home.path(), &default_git, &|_| {});
        assert!(r.migrated && r.errors.is_empty(), "{:?}", r.errors);
        let moved = state_dir(home.path()).join("worktrees/clone/job-1");
        assert_eq!(r.repaired, std::slice::from_ref(&moved));
        // The clone's own record now names the new location.
        let list = Command::new("git").args(["worktree", "list", "--porcelain"]).current_dir(&clone).output().unwrap();
        let listed = String::from_utf8_lossy(&list.stdout).replace("/private", "");
        assert!(listed.contains(&moved.to_string_lossy().replace("/private", "")), "{listed}");
    }

    #[test]
    fn should_migrate_runs_only_for_the_stable_install_and_never_when_the_operator_opted_out() {
        assert!(should_migrate(true, None));
        assert!(!should_migrate(false, None));
        assert!(!should_migrate(true, Some("1")));
        assert!(should_migrate(true, Some("0")));
    }
}
