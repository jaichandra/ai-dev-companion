//! The shared, persisted worktree-integrity baseline store (port of
//! `core/integrity-baselines.js`).
//!
//! resolve-conflict and address-review-comments take a snapshot of a
//! worktree's git metadata right before a Claude run and compare it after. If
//! the snapshot lived only in memory, a service restart (an auto-update, a
//! crash) would throw it away, and a later check for a job that survived the
//! restart would have nothing to compare against. This store also writes each
//! snapshot to `<dir>/<id>.integrity.json`, so a fresh process can rebuild its
//! cache from disk instead of trusting the worktree unchecked.
//!
//! The in-memory cache always wins over disk once a snapshot has been taken in
//! this process: `get` only reads the file when the id isn't already cached.
//! That keeps the guarantee this exists to protect: nothing that can write to
//! disk mid-run (including a resumed, unconfined Claude session) can replace
//! the snapshot this process took itself.
//!
//! It lives in the state dir's `jobs` folder rather than anywhere under the
//! worktree, because that is outside both the sandbox's write reach and the
//! edit guard's. A baseline sitting next to the thing it's meant to detect
//! tampering in would be tamperable by the exact same actor.

use crate::job_files::{baseline_file, write_json_atomic};
use crate::worktree_integrity::{snapshot_worktree_git_metadata, worktree_git_metadata_changed, GitMetadataSnapshot};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SNAPSHOT_KEYS: [&str; 4] = ["commondir", "configWorktree", "gitdir", "gitlink"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub git_dir: String,
    pub snapshot: GitMetadataSnapshot,
}

impl Baseline {
    fn to_json(&self) -> Value {
        json!({
            "gitDir": self.git_dir,
            "snapshot": {
                "gitlink": self.snapshot.gitlink,
                "commondir": self.snapshot.commondir,
                "gitdir": self.snapshot.gitdir,
                "configWorktree": self.snapshot.config_worktree,
            },
        })
    }

    /// A valid `{gitDir, snapshot}` entry: a non-empty string `gitDir`, plus a
    /// `snapshot` object with exactly the four keys, each `string | null`.
    /// Anything else (missing key, extra key, wrong type) is untrusted and
    /// treated as if the baseline were simply missing: a malformed file must
    /// never be trusted as "no change detected".
    fn from_json(v: &Value) -> Option<Self> {
        let git_dir = v["gitDir"].as_str().filter(|g| !g.is_empty())?;
        let snap = v["snapshot"].as_object()?;
        let mut keys: Vec<&str> = snap.keys().map(String::as_str).collect();
        keys.sort_unstable();
        if keys != SNAPSHOT_KEYS || !snap.values().all(|x| x.is_string() || x.is_null()) {
            return None;
        }
        let text = |k: &str| snap[k].as_str().map(String::from);
        Some(Baseline {
            git_dir: git_dir.to_string(),
            snapshot: GitMetadataSnapshot { gitlink: text("gitlink"), commondir: text("commondir"), gitdir: text("gitdir"), config_worktree: text("configWorktree") },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Intact,
    /// No (valid) baseline for this job at all.
    Missing,
    Changed,
}

#[derive(Default)]
pub struct BaselineStore {
    dir: Mutex<Option<PathBuf>>,
    cache: Mutex<HashMap<String, Baseline>>,
}

impl BaselineStore {
    /// `dir`, if set, is the directory baselines are persisted under; `None`
    /// means memory-only.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir: Mutex::new(dir), cache: Mutex::default() }
    }

    pub fn set_dir(&self, dir: Option<PathBuf>) {
        *self.dir.lock().unwrap() = dir;
    }

    fn file(&self, job_id: &str) -> Result<Option<String>, String> {
        match &*self.dir.lock().unwrap() {
            Some(d) => baseline_file(&d.to_string_lossy(), job_id).map(Some),
            None => Ok(None),
        }
    }

    /// Snapshots `{worktree_dir, git_dir}` now, writes it to disk if a dir is
    /// set, and only then caches it under `job_id`. Errors if that write fails:
    /// callers run this before any Claude run, and a baseline that can't survive
    /// a restart shouldn't silently let the job proceed as if it could. Written
    /// before cached so a failed write leaves nothing behind.
    pub fn take(&self, job_id: &str, worktree_dir: &Path, git_dir: &Path) -> Result<Baseline, String> {
        let snapshot = snapshot_worktree_git_metadata(worktree_dir, git_dir).map_err(|e| e.to_string())?;
        let entry = Baseline { git_dir: git_dir.to_string_lossy().into_owned(), snapshot };
        if let Some(file) = self.file(job_id)? {
            write_json_atomic(&file, &entry.to_json()).map_err(|e: io::Error| e.to_string())?;
        }
        self.cache.lock().unwrap().insert(job_id.to_string(), entry.clone());
        Ok(entry)
    }

    /// The baseline for `job_id`, or `None`. Checks the cache first; only when
    /// the id isn't cached does it read the file, and only when that file both
    /// parses and has the right shape. An unreadable or malformed file is
    /// treated exactly like a missing one.
    pub fn get(&self, job_id: &str) -> Option<Baseline> {
        if let Some(b) = self.cache.lock().unwrap().get(job_id) {
            return Some(b.clone());
        }
        let file = self.file(job_id).ok().flatten()?;
        let raw = fs::read(&file).ok()?;
        let entry = Baseline::from_json(&serde_json::from_str::<Value>(&String::from_utf8_lossy(&raw)).ok()?)?;
        self.cache.lock().unwrap().insert(job_id.to_string(), entry.clone());
        Some(entry)
    }

    /// `Intact` if `worktree_dir`'s current git metadata matches the baseline;
    /// `Missing` if there is no (valid) baseline; `Changed` otherwise,
    /// including when re-snapshotting itself fails (say one of the four paths
    /// became a directory). A read error is exactly the kind of thing this
    /// check exists to notice, so it must never be mistaken for "nothing changed".
    pub fn check(&self, job_id: &str, worktree_dir: &Path) -> Check {
        let Some(entry) = self.get(job_id) else { return Check::Missing };
        match snapshot_worktree_git_metadata(worktree_dir, Path::new(&entry.git_dir)) {
            Err(_) => Check::Changed,
            Ok(after) if worktree_git_metadata_changed(&entry.snapshot, &after) => Check::Changed,
            Ok(_) => Check::Intact,
        }
    }

    /// Drops `job_id` from the cache and deletes its file, if any.
    pub fn remove(&self, job_id: &str) -> Result<(), String> {
        self.cache.lock().unwrap().remove(job_id);
        if let Some(file) = self.file(job_id)? {
            match fs::remove_file(file) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.to_string()),
                _ => {}
            }
        }
        Ok(())
    }
}

/// The process-wide store both worktree features share.
pub fn baselines() -> &'static BaselineStore {
    static STORE: OnceLock<BaselineStore> = OnceLock::new();
    STORE.get_or_init(BaselineStore::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-1111-1111-1111-111111111111";
    const ID_2: &str = "22222222-2222-2222-2222-222222222222";

    /// A temp root laid out like a real worktree's private metadata:
    /// `<root>/wt/.git` (the gitlink file) and `<root>/gd/{commondir,gitdir}`
    /// (config.worktree deliberately absent).
    struct Fixture {
        root: tempfile::TempDir,
        wt: PathBuf,
        gd: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let (wt, gd) = (root.path().join("wt"), root.path().join("gd"));
        fs::create_dir_all(&wt).unwrap();
        fs::create_dir_all(&gd).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
        fs::write(gd.join("commondir"), "../..\n").unwrap();
        fs::write(gd.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
        Fixture { root, wt, gd }
    }

    fn jobs(f: &Fixture) -> PathBuf {
        f.root.path().join("jobs")
    }

    fn file(f: &Fixture) -> String {
        baseline_file(&jobs(f).to_string_lossy(), ID).unwrap()
    }

    fn tamper(f: &Fixture) {
        fs::write(f.wt.join(".git"), "gitdir: /somewhere/else\n").unwrap();
    }

    #[test]
    fn take_then_check_returns_intact() {
        let f = fixture();
        let store = BaselineStore::default();
        store.take(ID, &f.wt, &f.gd).unwrap();
        assert_eq!(store.check(ID, &f.wt), Check::Intact);
    }

    #[test]
    fn rewriting_the_worktrees_git_or_creating_config_worktree_makes_check_return_changed() {
        let f = fixture();
        let store = BaselineStore::default();
        store.take(ID, &f.wt, &f.gd).unwrap();
        tamper(&f);
        assert_eq!(store.check(ID, &f.wt), Check::Changed);
        let g = fixture();
        store.take(ID_2, &g.wt, &g.gd).unwrap();
        fs::write(g.gd.join("config.worktree"), "[core]\n\thooksPath = /tmp/evil\n").unwrap();
        assert_eq!(store.check(ID_2, &g.wt), Check::Changed);
    }

    #[test]
    fn check_on_an_unknown_id_returns_missing_and_ids_dont_collide() {
        let f = fixture();
        let store = BaselineStore::default();
        assert_eq!(store.check(ID, &f.wt), Check::Missing);
        store.take(ID, &f.wt, &f.gd).unwrap();
        assert_eq!(store.check(ID_2, &f.wt), Check::Missing);
        assert_eq!(store.check(ID, &f.wt), Check::Intact);
    }

    #[test]
    fn persistence_a_fresh_store_on_the_same_dir_sees_intact_then_changed_after_tampering_the_restart_case() {
        let f = fixture();
        BaselineStore::new(Some(jobs(&f))).take(ID, &f.wt, &f.gd).unwrap();
        assert_eq!(BaselineStore::new(Some(jobs(&f))).check(ID, &f.wt), Check::Intact);
        tamper(&f);
        assert_eq!(BaselineStore::new(Some(jobs(&f))).check(ID, &f.wt), Check::Changed);
    }

    #[test]
    fn the_baseline_file_sits_under_dir_not_under_the_worktree() {
        let f = fixture();
        let before: Vec<_> = fs::read_dir(&f.wt).unwrap().map(|e| e.unwrap().file_name()).collect();
        BaselineStore::new(Some(jobs(&f))).take(ID, &f.wt, &f.gd).unwrap();
        let after: Vec<_> = fs::read_dir(&f.wt).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(after, before);
        assert!(Path::new(&file(&f)).exists());
        assert!(file(&f).ends_with(&format!("{ID}.integrity.json")));
    }

    #[test]
    fn the_cache_wins_a_disk_file_rewritten_to_match_a_tampered_worktree_doesnt_fool_the_same_store() {
        let f = fixture();
        let store = BaselineStore::new(Some(jobs(&f)));
        store.take(ID, &f.wt, &f.gd).unwrap();
        tamper(&f);
        // Overwrite the on-disk baseline with one that matches the tampered worktree: if check()
        // consulted disk instead of the cache, it would now (wrongly) say intact.
        let tampered = json!({ "gitDir": f.gd, "snapshot": { "gitlink": "gitdir: /somewhere/else\n", "commondir": "../..\n", "gitdir": format!("{}/.git\n", f.wt.display()), "configWorktree": null } });
        fs::write(file(&f), tampered.to_string()).unwrap();
        assert_eq!(store.check(ID, &f.wt), Check::Changed);
        assert_eq!(BaselineStore::new(Some(jobs(&f))).check(ID, &f.wt), Check::Intact, "a fresh process would trust the file: that is why it lives outside the worktree");
    }

    #[test]
    fn a_truncated_or_wrongly_shaped_file_makes_a_fresh_store_return_missing() {
        let f = fixture();
        BaselineStore::new(Some(jobs(&f))).take(ID, &f.wt, &f.gd).unwrap();
        let gd = f.gd.to_string_lossy().into_owned();
        let bad = [
            "{ not json".to_string(),
            json!({ "gitDir": gd, "snapshot": { "gitlink": "x", "commondir": "y", "gitdir": "z" } }).to_string(),
            json!({ "gitDir": gd, "snapshot": { "gitlink": "x", "commondir": "y", "gitdir": "z", "configWorktree": 1 } }).to_string(),
            json!({ "gitDir": gd, "snapshot": { "gitlink": "x", "commondir": "y", "gitdir": "z", "configWorktree": null, "extra": null } }).to_string(),
            json!({ "gitDir": "", "snapshot": { "gitlink": "x", "commondir": "y", "gitdir": "z", "configWorktree": null } }).to_string(),
            json!({ "snapshot": { "gitlink": "x", "commondir": "y", "gitdir": "z", "configWorktree": null } }).to_string(),
            "null".to_string(),
        ];
        for content in bad {
            fs::write(file(&f), &content).unwrap();
            let fresh = BaselineStore::new(Some(jobs(&f)));
            assert_eq!(fresh.get(ID), None, "{content}");
            assert_eq!(fresh.check(ID, &f.wt), Check::Missing, "{content}");
        }
    }

    #[test]
    fn remove_deletes_the_file_and_a_fresh_store_then_returns_missing() {
        let f = fixture();
        let a = BaselineStore::new(Some(jobs(&f)));
        a.take(ID, &f.wt, &f.gd).unwrap();
        a.remove(ID).unwrap();
        assert!(!Path::new(&file(&f)).exists());
        assert_eq!(a.get(ID), None);
        assert_eq!(BaselineStore::new(Some(jobs(&f))).check(ID, &f.wt), Check::Missing);
        a.remove(ID).unwrap();
    }

    #[test]
    fn with_no_dir_set_take_and_check_work_and_no_file_is_written_set_dir_turns_persistence_on() {
        let f = fixture();
        let before = fs::read_dir(f.root.path()).unwrap().count();
        let store = BaselineStore::default();
        store.take(ID, &f.wt, &f.gd).unwrap();
        assert_eq!(store.check(ID, &f.wt), Check::Intact);
        assert_eq!(fs::read_dir(f.root.path()).unwrap().count(), before);
        store.set_dir(Some(jobs(&f)));
        store.take(ID, &f.wt, &f.gd).unwrap();
        assert!(Path::new(&file(&f)).exists());
    }

    #[test]
    fn take_errors_when_the_dir_isnt_writable_and_a_failed_take_caches_nothing() {
        let f = fixture();
        let not_a_dir = f.root.path().join("not-a-dir");
        fs::write(&not_a_dir, "im a file, not a directory\n").unwrap();
        let store = BaselineStore::new(Some(not_a_dir));
        assert!(store.take(ID, &f.wt, &f.gd).is_err());
        // Same store, same process: a baseline that never reached disk must not be trusted from the
        // cache either, or it would vanish on restart.
        assert_eq!(store.check(ID, &f.wt), Check::Missing);
    }

    #[test]
    fn a_snapshot_read_error_gd_gitdir_is_a_directory_makes_check_return_changed() {
        let f = fixture();
        let store = BaselineStore::default();
        store.take(ID, &f.wt, &f.gd).unwrap();
        fs::remove_file(f.gd.join("gitdir")).unwrap();
        fs::create_dir(f.gd.join("gitdir")).unwrap();
        assert_eq!(store.check(ID, &f.wt), Check::Changed);
    }

    #[test]
    fn a_job_id_that_isnt_a_uuid_is_refused_when_persisting_and_the_singleton_is_usable() {
        let f = fixture();
        let store = BaselineStore::new(Some(jobs(&f)));
        assert!(store.take("../x", &f.wt, &f.gd).is_err());
        assert_eq!(store.get("../x"), None);
        let b = baselines();
        b.take(ID, &f.wt, &f.gd).unwrap();
        assert_eq!(b.check(ID, &f.wt), Check::Intact);
        b.remove(ID).unwrap();
    }
}
