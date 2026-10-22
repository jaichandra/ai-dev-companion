//! Restart-resilient jobs: the on-disk layer under the job store (port of
//! `core/job-files.js`). The store is in memory only, so a service restart
//! mid-job would silently drop whatever the user was reviewing, including a job
//! stuck "running" with no way to tell if it finished. This module saves and
//! loads one JSON file per job, reconciles anything caught mid-flight to a
//! clear "failed" state, and prunes what is old enough that nobody is coming
//! back for it.
//!
//! Only features a caller opts in (`persist_action`'s `feature_ids`) get their
//! jobs written to disk at all. Loading is deliberately paranoid, because the
//! files are read back blind at startup: every id is checked against a UUID's
//! shape before it builds a path, and every `data.worktree.dir` must still be
//! exactly `<worktreeRoot>/<repo>/<id>` before anything trusts it.

use crate::ids::is_lower_uuid;
use crate::js_number::json_number;
use crate::node_path;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// How long a terminal job's files are kept before `load_jobs` prunes them.
pub const PRUNE_AFTER_MS: f64 = 14.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// Statuses that only make sense while this process is the one driving them;
/// anything left in one after a restart was cut off mid-step. "pending-start"
/// isn't here: it has no worktree and is not driven by this process.
pub const INTERRUPTED_STATUSES: [&str; 3] = ["running", "approving", "rejecting"];

pub const RESTART_MESSAGE: &str = "The service restarted while this job was in progress (service restarted — click Refresh diff).";

/// For an interrupted job that never created a worktree: there is nothing for
/// Refresh diff to pick up.
pub const RESTART_MESSAGE_NO_WORKTREE: &str = "The service restarted before this job had anything to review (service restarted — start it again).";

/// The jobs of `feature_ids` that are written to disk: those whose feature
/// does not opt out with `persist: false`.
pub fn feature_ids_to_persist<'a>(feature_ids: &[&'a str], not_persisted: &[&str]) -> Vec<&'a str> {
    feature_ids.iter().copied().filter(|id| !not_persisted.contains(id)).collect()
}

/// A job nobody acted on for `max_age_ms`: only awaiting-approval and failed
/// jobs can be stale; a running one is never swept.
pub fn is_stale(job: &Value, now: f64, max_age_ms: f64) -> bool {
    if !matches!(job["status"].as_str(), Some("awaiting-approval" | "failed")) {
        return false;
    }
    job["updatedAt"].as_f64().filter(|u| u.is_finite()).is_some_and(|u| now - u > max_age_ms)
}

pub fn jobs_dir(state_dir: &str) -> String {
    node_path::join(&[state_dir, "jobs"])
}

fn assert_job_id(id: &str) -> Result<(), String> {
    if is_lower_uuid(id) { Ok(()) } else { Err(format!("Not a job id: {id}")) }
}

/// `<dir>/<id>.json`; errors on a non-UUID id, since it is about to become a
/// filename (path safety, not just validation).
pub fn job_file(dir: &str, id: &str) -> Result<String, String> {
    assert_job_id(id)?;
    Ok(node_path::join(&[dir, &format!("{id}.json")]))
}

/// `<dir>/<id>.integrity.json`: where the integrity-baseline store keeps one
/// job's snapshot, next to its job file.
pub fn baseline_file(dir: &str, id: &str) -> Result<String, String> {
    assert_job_id(id)?;
    Ok(node_path::join(&[dir, &format!("{id}.integrity.json")]))
}

/// Writes `value` as JSON to `file`, atomically: `mkdir -p` its directory
/// (0700), write a per-process tmp file (0600) beside it, rename it over the
/// real path. A reader never sees a half-written file; a crash mid-write
/// leaves at most a stray `<file>.<pid>.tmp`.
pub fn write_json_atomic(file: &str, value: &Value) -> io::Result<()> {
    let dir = node_path::dirname(file);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let tmp = format!("{file}.{}.tmp", std::process::id());
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    fs::rename(&tmp, file)
}

pub fn save_job(dir: &str, job: &Value) -> io::Result<()> {
    let id = job["id"].as_str().unwrap_or("");
    write_json_atomic(&job_file(dir, id).map_err(io::Error::other)?, job)
}

fn rm(path: &str) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Removes both of `id`'s files (job + baseline), if present; a no-op when
/// neither exists.
pub fn remove_job(dir: &str, id: &str) -> Result<(), String> {
    rm(&job_file(dir, id)?).map_err(|e| e.to_string())?;
    rm(&baseline_file(dir, id)?).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistAction {
    Skip,
    Remove,
    Save,
}

/// Whether, and how, a job should be persisted: `Skip` if its feature hasn't
/// opted in; `Remove` once it reached a terminal state a restart can't affect
/// (approved or rejected); `Save` otherwise.
pub fn persist_action(job: &Value, feature_ids: &[&str]) -> PersistAction {
    if !job["featureId"].as_str().is_some_and(|f| feature_ids.contains(&f)) {
        return PersistAction::Skip;
    }
    match job["status"].as_str() {
        Some("approved" | "rejected") => PersistAction::Remove,
        _ => PersistAction::Save,
    }
}

/// Whether `target` resolves to strictly inside `root`: not equal to it, and
/// not escaping it via `..`. Like the Node original, a child whose name starts
/// with `..` counts as outside.
pub fn is_strictly_inside(root: Option<&str>, target: Option<&str>) -> bool {
    let (Some(root), Some(target)) = (root, target) else { return false };
    let (root, target) = (node_path::resolve(root), node_path::resolve(target));
    let prefix = if root == "/" { "/".to_string() } else { format!("{root}/") };
    match target.strip_prefix(&prefix) {
        Some(rel) => !rel.is_empty() && !rel.starts_with(".."),
        None => false,
    }
}

/// Whether `dir` is exactly `<worktreeRoot>/<repo-slug>/<id>`. Stricter than
/// `is_strictly_inside` on purpose: discard may `rm -rf` this path.
pub fn is_job_worktree_dir(worktree_root: Option<&str>, dir: Option<&str>, id: &str) -> bool {
    let Some(dir) = dir.filter(|d| !d.is_empty()) else { return false };
    if !is_strictly_inside(worktree_root, Some(dir)) {
        return false;
    }
    let resolved = node_path::resolve(dir);
    node_path::basename(&resolved) == id && node_path::dirname(&node_path::dirname(&resolved)) == node_path::resolve(worktree_root.unwrap_or(""))
}

pub struct LoadOptions<'a> {
    pub now: f64,
    pub worktree_root: Option<&'a str>,
    pub max_age_ms: f64,
    pub log: &'a dyn Fn(&str),
    /// Whether a Claude session's cwd is a directory this service would have
    /// used, for a job without a worktree. Refuses everything by default.
    pub session_cwd_ok: &'a dyn Fn(&str) -> bool,
    /// Whether a reused (adopted) worktree is a linked worktree of a
    /// configured repo: `(repo_path, dir)`. Refuses everything by default.
    pub adopted_worktree_ok: &'a dyn Fn(&str, &str) -> bool,
}

impl Default for LoadOptions<'_> {
    fn default() -> Self {
        Self {
            now: now_ms(),
            worktree_root: None,
            max_age_ms: PRUNE_AFTER_MS,
            log: &|_| {},
            session_cwd_ok: &|_| false,
            adopted_worktree_ok: &|_, _| false,
        }
    }
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub jobs: Vec<Value>,
    pub reconciled: Vec<String>,
    pub pruned: Vec<String>,
    pub pruned_jobs: Vec<Value>,
    pub dropped: Vec<String>,
}

/// Loads every persisted job from `dir`, reconciling interrupted statuses and
/// pruning stale ones as it goes. Never fails: a missing `dir`, an unreadable
/// or malformed file, or an unsafe path are skipped or dropped, so one bad
/// file can't keep the rest of a user's jobs from loading at startup.
///
/// - Reads every `*.json` that isn't `*.integrity.json` (tmp files from an
///   interrupted write never end in `.json`).
/// - A parse error, an `id` that isn't a UUID, or an `id` that doesn't match
///   its file name: the file is removed and the file-name id is `dropped`.
/// - An `updatedAt` that isn't a finite number: same (it would never age out).
/// - A `data.worktree` whose `dir` isn't exactly `<worktreeRoot>/<repo>/<id>`
///   (or an adopted worktree `adopted_worktree_ok` vouches for), or a
///   `data.claudeSession` whose `cwd` isn't that same dir (or, without a
///   worktree, isn't one `session_cwd_ok` allows): the job and its baseline
///   are removed and `dropped`.
/// - Older than `max_age_ms` (by `updatedAt`): removed and `pruned`; its
///   worktree is left, removing it is the caller's job.
/// - In an interrupted status: rewritten to `failed` with a restart message,
///   no `progress`, a fresh `updatedAt`, saved back (a failed save is only
///   logged) and `reconciled`.
/// - Finally any `<id>.integrity.json` with no `<id>.json` is removed.
pub fn load_jobs(dir: &str, opts: &LoadOptions<'_>) -> Loaded {
    let mut result = Loaded::default();
    let log = opts.log;
    let Ok(read) = fs::read_dir(dir) else { return result };
    let mut names: Vec<String> = read.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let remove_file = |file: &str| {
        if let Err(e) = rm(file) {
            log(&format!("[jobs] could not remove {file}: {e}"));
        }
    };
    let remove_job = |id: &str| {
        if let Err(e) = remove_job(dir, id) {
            log(&format!("[jobs] could not remove job {id}: {e}"));
        }
    };

    for name in &names {
        if !name.ends_with(".json") || name.ends_with(".integrity.json") {
            continue;
        }
        let file_id = &name[..name.len() - ".json".len()];
        let file = node_path::join(&[dir, name]);

        let parsed = fs::read(&file).map_err(|e| e.to_string()).and_then(|b| serde_json::from_str::<Value>(&String::from_utf8_lossy(&b)).map_err(|e| e.to_string()));
        let mut job = match parsed {
            Ok(job) if job.is_object() => job,
            other => {
                let why = other.err().unwrap_or_else(|| "not a JSON object".into());
                log(&format!("[jobs] dropping {name}: {why}"));
                remove_file(&file);
                result.dropped.push(file_id.to_string());
                continue;
            }
        };

        let id = match job["id"].as_str() {
            Some(id) if is_lower_uuid(id) && id == file_id => id.to_string(),
            _ => {
                log(&format!("[jobs] dropping {name}: id is missing, invalid, or doesn't match the file name"));
                remove_file(&file);
                result.dropped.push(file_id.to_string());
                continue;
            }
        };

        let Some(updated_at) = job["updatedAt"].as_f64().filter(|u| u.is_finite()) else {
            log(&format!("[jobs] dropping {name}: updatedAt is missing or not a number"));
            remove_file(&file);
            result.dropped.push(id);
            continue;
        };

        let worktree = job.get("data").and_then(|d| d.get("worktree")).cloned();
        let worktree_dir = worktree.as_ref().and_then(|w| w.get("dir")).and_then(Value::as_str).map(String::from);

        // Any `data.worktree` at all must name exactly this job's own worktree
        // (an empty or missing `dir` included, since discard will try to
        // remove it), or be a reused one `adopted_worktree_ok` vouches for;
        // such a worktree is never removed with the job.
        let adopted_ok = worktree.as_ref().is_some_and(|w| w["adopted"] == json!(true))
            && match (job["data"]["repoPath"].as_str(), worktree_dir.as_deref()) {
                (Some(repo), Some(dir)) => (opts.adopted_worktree_ok)(repo, dir),
                _ => false,
            };
        if worktree.is_some() && !adopted_ok && !is_job_worktree_dir(opts.worktree_root, worktree_dir.as_deref(), &id) {
            log(&format!(
                "[jobs] dropping job {id}: its worktree ({}) isn't {}/<repo>/{id}",
                worktree_dir.as_deref().unwrap_or("undefined"),
                opts.worktree_root.unwrap_or("undefined")
            ));
            remove_job(&id);
            result.dropped.push(id);
            continue;
        }

        // A recorded Claude session is resumed in its `cwd`. With a worktree it
        // must be exactly that worktree; without one it must be a directory
        // this service would have used.
        if let Some(session) = job.get("data").and_then(|d| d.get("claudeSession")) {
            let session_cwd = session.get("cwd").and_then(Value::as_str);
            let ok = if worktree.is_some() { session_cwd.is_some() && session_cwd == worktree_dir.as_deref() } else { session_cwd.is_some_and(|c| (opts.session_cwd_ok)(c)) };
            if !ok {
                log(&format!("[jobs] dropping job {id}: its Claude session's cwd isn't an allowed directory"));
                remove_job(&id);
                result.dropped.push(id);
                continue;
            }
        }

        if opts.now - updated_at > opts.max_age_ms {
            remove_job(&id);
            if let Some(wd) = worktree_dir.as_deref().filter(|w| Path::new(w).exists()) {
                log(&format!("[jobs] pruned stale job {id}; its worktree is left at {wd}"));
            }
            result.pruned.push(id);
            result.pruned_jobs.push(job);
            continue;
        }

        if job["status"].as_str().is_some_and(|s| INTERRUPTED_STATUSES.contains(&s)) {
            let obj = job.as_object_mut().unwrap();
            obj.insert("status".into(), json!("failed"));
            obj.insert("error".into(), json!(if worktree.is_none() { RESTART_MESSAGE_NO_WORKTREE } else { RESTART_MESSAGE }));
            obj.shift_remove("progress");
            obj.insert("updatedAt".into(), json_number(opts.now));
            // The in-memory reconciliation must survive even if writing it back
            // doesn't (EACCES, ENOSPC): only the on-disk copy may be lost.
            if let Err(e) = save_job(dir, &job) {
                log(&format!("[jobs] could not save reconciled job {id}: {e}"));
            }
            result.reconciled.push(id);
        }

        result.jobs.push(job);
    }

    for name in &names {
        let Some(id) = name.strip_suffix(".integrity.json") else { continue };
        if !Path::new(&node_path::join(&[dir, &format!("{id}.json")])).exists() {
            remove_file(&node_path::join(&[dir, name]));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    const ID: &str = "11111111-1111-1111-1111-111111111111";
    const ID_2: &str = "22222222-2222-2222-2222-222222222222";
    const ID_3: &str = "33333333-3333-3333-3333-333333333333";
    const ID_4: &str = "44444444-4444-4444-4444-444444444444";
    const ID_5: &str = "55555555-5555-5555-5555-555555555555";

    struct Root(tempfile::TempDir);
    impl Root {
        fn new() -> Self {
            Root(tempfile::tempdir().unwrap())
        }
        fn path(&self) -> String {
            // macOS temp dirs sit behind a /var -> /private/var link; resolve it once.
            fs::canonicalize(self.0.path()).unwrap().to_string_lossy().into_owned()
        }
        fn dir(&self) -> String {
            jobs_dir(&self.path())
        }
        fn sub(&self, name: &str) -> String {
            node_path::join(&[&self.path(), name])
        }
    }

    fn job(over: Value) -> Value {
        let mut j = json!({ "id": ID, "featureId": "review-in-editor", "status": "awaiting-approval", "data": {}, "createdAt": 1000, "updatedAt": 1000 });
        for (k, v) in over.as_object().unwrap() {
            if v.is_null() && k == "updatedAt" {
                j.as_object_mut().unwrap().remove(k);
            } else {
                j[k] = v.clone();
            }
        }
        j
    }

    fn opts<'a>(now: f64, worktree_root: Option<&'a str>) -> LoadOptions<'a> {
        LoadOptions { now, worktree_root, ..Default::default() }
    }

    fn worktree_data(worktree_root: &str, id: &str) -> Value {
        json!({ "worktree": { "dir": node_path::join(&[worktree_root, "some-repo", id]) } })
    }

    fn exists(p: &str) -> bool {
        Path::new(p).exists()
    }

    fn mode(p: &str) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn save_job_then_load_jobs_round_trips_an_awaiting_approval_job_unchanged() {
        let root = Root::new();
        let dir = root.dir();
        let j = job(json!({}));
        save_job(&dir, &j).unwrap();
        let result = load_jobs(&dir, &opts(2000.0, Some(&root.path())));
        assert_eq!(result, Loaded { jobs: vec![j.clone()], ..Default::default() });
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&job_file(&dir, ID).unwrap()), 0o600);
    }

    #[test]
    fn load_jobs_on_a_missing_dir_returns_empty_lists() {
        let root = Root::new();
        assert_eq!(load_jobs(&root.dir(), &opts(0.0, Some(&root.path()))), Loaded::default());
    }

    #[test]
    fn load_jobs_reconciles_an_interrupted_job_to_failed_with_the_restart_message_and_drops_progress() {
        for status in ["running", "approving", "rejecting"] {
            let root = Root::new();
            let dir = root.dir();
            let wt = root.sub("worktrees");
            let j = job(json!({ "status": status, "data": worktree_data(&wt, ID), "progress": { "stepId": "merge", "label": "Merging" } }));
            save_job(&dir, &j).unwrap();
            let result = load_jobs(&dir, &opts(2000.0, Some(&wt)));
            assert_eq!(result.jobs.len(), 1, "{status}");
            let r = &result.jobs[0];
            assert_eq!(r["status"], "failed");
            assert_eq!(r["error"], RESTART_MESSAGE);
            assert!(r["error"].as_str().unwrap().contains("service restarted — click Refresh diff"));
            assert!(r.get("progress").is_none());
            assert_eq!(r["updatedAt"], 2000);
            assert_eq!(result.reconciled, [ID]);
            let reload = load_jobs(&dir, &opts(3000.0, Some(&wt)));
            assert_eq!(reload.jobs, result.jobs);
        }
    }

    #[test]
    fn an_interrupted_job_with_no_worktree_gets_the_no_worktree_message() {
        let root = Root::new();
        let dir = root.dir();
        save_job(&dir, &job(json!({ "status": "running", "progress": { "stepId": "fetch", "label": "Fetching" } }))).unwrap();
        let result = load_jobs(&dir, &opts(2000.0, Some(&root.sub("worktrees"))));
        assert_eq!(RESTART_MESSAGE_NO_WORKTREE, "The service restarted before this job had anything to review (service restarted — start it again).");
        assert_eq!(result.jobs[0]["status"], "failed");
        assert_eq!(result.jobs[0]["error"], RESTART_MESSAGE_NO_WORKTREE);
        assert!(result.jobs[0].get("progress").is_none());
        assert_eq!(result.reconciled, [ID]);
    }

    #[test]
    fn reconciling_keeps_the_job_keys_in_their_original_order() {
        let root = Root::new();
        let dir = root.dir();
        save_job(&dir, &job(json!({ "status": "running", "progress": { "stepId": "x" }, "result": { "a": 1 } }))).unwrap();
        let r = load_jobs(&dir, &opts(2000.0, Some(&root.path())));
        let keys: Vec<&str> = r.jobs[0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "featureId", "status", "data", "createdAt", "updatedAt", "result", "error"]);
    }

    #[test]
    fn a_stale_job_is_pruned_both_files_disappear_but_its_worktree_dir_is_left_on_disk() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        let worktree_dir = node_path::join(&[&wt, "some-repo", ID]);
        fs::create_dir_all(&worktree_dir).unwrap();
        let now = 100_000_000.0;
        let j = job(json!({ "data": { "worktree": { "dir": worktree_dir } }, "updatedAt": now - PRUNE_AFTER_MS - 1.0 }));
        save_job(&dir, &j).unwrap();
        write_json_atomic(&baseline_file(&dir, ID).unwrap(), &json!({ "some": "baseline" })).unwrap();
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_string());
        let result = load_jobs(&dir, &LoadOptions { now, worktree_root: Some(&wt), log: &log, ..Default::default() });
        assert!(result.jobs.is_empty());
        assert_eq!(result.pruned, [ID]);
        assert_eq!(result.pruned_jobs, [j]);
        assert!(!exists(&job_file(&dir, ID).unwrap()) && !exists(&baseline_file(&dir, ID).unwrap()));
        assert!(exists(&worktree_dir));
        assert!(logged.lock().unwrap().iter().any(|m| m.contains(&format!("pruned stale job {ID}")) && m.contains(&worktree_dir)));
    }

    #[test]
    fn a_job_whose_worktree_dir_is_outside_the_root_is_dropped_sibling_root_itself_and_dotdot_escape() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        fs::create_dir_all(&wt).unwrap();
        let cases = [(ID, root.sub("sibling")), (ID_2, wt.clone()), (ID_3, format!("{wt}/../escape"))];
        for (id, bad) in &cases {
            save_job(&dir, &job(json!({ "id": id, "data": { "worktree": { "dir": bad } } }))).unwrap();
        }
        let result = load_jobs(&dir, &opts(2000.0, Some(&wt)));
        assert!(result.jobs.is_empty());
        assert_eq!(sorted(result.dropped), [ID, ID_2, ID_3]);
        for (id, _) in &cases {
            assert!(!exists(&job_file(&dir, id).unwrap()));
        }
    }

    #[test]
    fn a_worktree_in_the_real_layout_with_a_matching_claude_session_cwd_loads() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        let mut data = worktree_data(&wt, ID);
        let wdir = data["worktree"]["dir"].clone();
        data["claudeSession"] = json!({ "id": "s", "cwd": wdir, "permissionMode": "default" });
        let j = job(json!({ "data": data }));
        save_job(&dir, &j).unwrap();
        let result = load_jobs(&dir, &opts(2000.0, Some(&wt)));
        assert_eq!(result.jobs, [j]);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn a_worktree_that_isnt_exactly_root_repo_id_is_dropped_along_with_its_baseline() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        let cases = [
            (ID, json!({ "worktree": { "dir": "" } })),
            (ID_2, json!({ "worktree": {} })),
            (ID_3, json!({ "worktree": { "dir": node_path::join(&[&wt, "some-repo", ID_5]) } })),
            (ID_4, json!({ "worktree": { "dir": node_path::join(&[&wt, ID_4]) } })),
        ];
        for (id, data) in &cases {
            save_job(&dir, &job(json!({ "id": id, "data": data }))).unwrap();
            write_json_atomic(&baseline_file(&dir, id).unwrap(), &json!({ "some": "baseline" })).unwrap();
        }
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_string());
        let result = load_jobs(&dir, &LoadOptions { now: 2000.0, worktree_root: Some(&wt), log: &log, ..Default::default() });
        assert!(result.jobs.is_empty());
        assert_eq!(sorted(result.dropped), [ID, ID_2, ID_3, ID_4]);
        for (id, _) in &cases {
            assert!(!exists(&job_file(&dir, id).unwrap()) && !exists(&baseline_file(&dir, id).unwrap()));
        }
        assert!(logged.lock().unwrap().len() >= cases.len());
    }

    #[test]
    fn a_claude_session_whose_cwd_isnt_the_jobs_worktree_dir_is_dropped_along_with_its_baseline() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        let mut mismatched = worktree_data(&wt, ID);
        mismatched["claudeSession"] = json!({ "id": "s", "cwd": root.sub("elsewhere"), "permissionMode": "default" });
        let no_worktree = json!({ "claudeSession": { "id": "s", "cwd": node_path::join(&[&wt, "some-repo", ID_2]), "permissionMode": "default" } });
        save_job(&dir, &job(json!({ "id": ID, "data": mismatched }))).unwrap();
        save_job(&dir, &job(json!({ "id": ID_2, "data": no_worktree }))).unwrap();
        write_json_atomic(&baseline_file(&dir, ID).unwrap(), &json!({ "some": "baseline" })).unwrap();
        let result = load_jobs(&dir, &opts(2000.0, Some(&wt)));
        assert!(result.jobs.is_empty());
        assert_eq!(sorted(result.dropped), [ID, ID_2]);
        assert!(!exists(&job_file(&dir, ID).unwrap()) && !exists(&baseline_file(&dir, ID).unwrap()) && !exists(&job_file(&dir, ID_2).unwrap()));
    }

    #[test]
    fn a_session_without_a_worktree_loads_when_session_cwd_ok_allows_its_directory() {
        let root = Root::new();
        let dir = root.dir();
        let j = job(json!({ "data": { "claudeSession": { "id": "s", "cwd": "/allowed/repo" } } }));
        save_job(&dir, &j).unwrap();
        let ok = |c: &str| c == "/allowed/repo";
        let r = load_jobs(&dir, &LoadOptions { now: 2000.0, worktree_root: Some(&root.path()), session_cwd_ok: &ok, ..Default::default() });
        assert_eq!(r.jobs, [j]);
    }

    #[test]
    fn garbage_json_a_non_uuid_id_and_a_name_id_mismatch_are_each_dropped_other_valid_jobs_still_load() {
        let root = Root::new();
        let dir = root.dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(format!("{dir}/{ID}.json"), "not json{").unwrap();
        fs::write(format!("{dir}/not-a-uuid.json"), serde_json::to_string(&job(json!({ "id": "not-a-uuid" }))).unwrap()).unwrap();
        fs::write(format!("{dir}/{ID_2}.json"), serde_json::to_string(&job(json!({ "id": ID_3 }))).unwrap()).unwrap();
        fs::write(format!("{dir}/{ID_5}.json"), "[1,2]").unwrap();
        save_job(&dir, &job(json!({ "id": ID_4 }))).unwrap();
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_string());
        let result = load_jobs(&dir, &LoadOptions { now: 2000.0, worktree_root: Some(&root.path()), log: &log, ..Default::default() });
        assert_eq!(result.jobs, [job(json!({ "id": ID_4 }))]);
        assert_eq!(sorted(result.dropped), sorted(vec![ID.into(), "not-a-uuid".into(), ID_2.into(), ID_5.into()]));
        for n in [ID, "not-a-uuid", ID_2, ID_5] {
            assert!(!exists(&format!("{dir}/{n}.json")), "{n}");
        }
        assert!(logged.lock().unwrap().len() >= 4);
    }

    #[test]
    fn a_missing_worktree_root_drops_a_job_with_a_worktree_but_other_valid_jobs_load() {
        let root = Root::new();
        let dir = root.dir();
        save_job(&dir, &job(json!({ "id": ID, "data": { "worktree": { "dir": root.sub("somewhere") } } }))).unwrap();
        let plain = job(json!({ "id": ID_2 }));
        save_job(&dir, &plain).unwrap();
        let result = load_jobs(&dir, &opts(2000.0, None));
        assert_eq!(result.jobs, [plain]);
        assert_eq!(result.dropped, [ID]);
        assert!(!exists(&job_file(&dir, ID).unwrap()));
    }

    #[test]
    fn a_failing_reconcile_save_is_logged_and_the_job_still_comes_back_failed() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        let j = job(json!({ "status": "running", "data": worktree_data(&wt, ID), "progress": { "stepId": "merge" } }));
        save_job(&dir, &j).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_string());
        let result = load_jobs(&dir, &LoadOptions { now: 2000.0, worktree_root: Some(&wt), log: &log, ..Default::default() });
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.jobs.len(), 1);
        assert_eq!(result.jobs[0]["status"], "failed");
        assert_eq!(result.reconciled, [ID]);
        assert!(logged.lock().unwrap().iter().any(|m| m.contains(ID)));
    }

    #[test]
    fn a_job_whose_updated_at_isnt_a_finite_number_is_dropped_and_its_files_removed() {
        let root = Root::new();
        let dir = root.dir();
        fs::create_dir_all(&dir).unwrap();
        save_job(&dir, &job(json!({ "id": ID, "updatedAt": null }))).unwrap();
        fs::write(format!("{dir}/{ID_2}.json"), serde_json::to_string(&job(json!({ "id": ID_2, "updatedAt": "5" }))).unwrap()).unwrap();
        save_job(&dir, &job(json!({ "id": ID_3 }))).unwrap();
        let result = load_jobs(&dir, &opts(2000.0, Some(&root.path())));
        assert_eq!(result.jobs, [job(json!({ "id": ID_3 }))]);
        assert_eq!(sorted(result.dropped), [ID, ID_2]);
        assert!(!exists(&job_file(&dir, ID).unwrap()) && !exists(&job_file(&dir, ID_2).unwrap()));
    }

    #[test]
    fn an_orphan_baseline_is_removed_and_a_baseline_with_a_job_file_is_kept() {
        let root = Root::new();
        let dir = root.dir();
        save_job(&dir, &job(json!({ "id": ID }))).unwrap();
        write_json_atomic(&baseline_file(&dir, ID).unwrap(), &json!({ "kept": true })).unwrap();
        write_json_atomic(&baseline_file(&dir, ID_2).unwrap(), &json!({ "orphan": true })).unwrap();
        load_jobs(&dir, &opts(2000.0, Some(&root.path())));
        assert!(exists(&baseline_file(&dir, ID).unwrap()));
        assert!(!exists(&baseline_file(&dir, ID_2).unwrap()));
    }

    #[test]
    fn remove_job_removes_both_files_and_is_fine_when_neither_exists() {
        let root = Root::new();
        let dir = root.dir();
        save_job(&dir, &job(json!({ "id": ID_5 }))).unwrap();
        write_json_atomic(&baseline_file(&dir, ID_5).unwrap(), &json!({ "a": 1 })).unwrap();
        remove_job(&dir, ID_5).unwrap();
        assert!(!exists(&job_file(&dir, ID_5).unwrap()) && !exists(&baseline_file(&dir, ID_5).unwrap()));
        remove_job(&dir, ID_5).unwrap();
        assert!(remove_job(&dir, "../x").is_err());
    }

    #[test]
    fn job_file_and_baseline_file_reject_a_non_uuid_id_and_name_the_right_files() {
        assert!(job_file("/tmp/whatever", "../x").is_err());
        assert!(baseline_file("/tmp/whatever", "../x").is_err());
        assert!(job_file("/tmp/whatever", "ABCDEF01-1111-1111-1111-111111111111").is_err(), "lowercase only");
        assert_eq!(baseline_file("/tmp/whatever", ID).unwrap(), format!("/tmp/whatever/{ID}.integrity.json"));
        assert_eq!(job_file("/tmp/whatever", ID).unwrap(), format!("/tmp/whatever/{ID}.json"));
    }

    #[test]
    fn persist_action_returns_skip_save_and_remove_as_specified() {
        let ids = ["review-in-editor"];
        assert_eq!(persist_action(&job(json!({ "featureId": "other-feature" })), &ids), PersistAction::Skip);
        assert_eq!(persist_action(&job(json!({ "status": "approved" })), &ids), PersistAction::Remove);
        assert_eq!(persist_action(&job(json!({ "status": "rejected" })), &ids), PersistAction::Remove);
        assert_eq!(persist_action(&job(json!({ "status": "awaiting-approval" })), &ids), PersistAction::Save);
        assert_eq!(persist_action(&job(json!({ "status": "running" })), &ids), PersistAction::Save);
    }

    #[test]
    fn write_json_atomic_leaves_no_tmp_file_and_sets_the_modes() {
        let root = Root::new();
        let file = root.sub("sub/thing.json");
        write_json_atomic(&file, &json!({ "a": 1 })).unwrap();
        let names: Vec<String> = fs::read_dir(node_path::dirname(&file)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
        assert_eq!(serde_json::from_str::<Value>(&fs::read_to_string(&file).unwrap()).unwrap(), json!({ "a": 1 }));
        assert_eq!(mode(&node_path::dirname(&file)), 0o700);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\n  \"a\": 1\n}", "JSON.stringify(v, null, 2) layout, no trailing newline");
    }

    #[test]
    fn a_job_that_reused_an_existing_worktree_loads_only_when_adopted_worktree_ok_vouches_for_it() {
        let root = Root::new();
        let dir = root.dir();
        let wt = root.sub("worktrees");
        let mine = root.sub("my-own-tree");
        let data = json!({
            "repoPath": "/r/app",
            "worktree": { "id": ID, "dir": mine, "repoPath": "/r/app", "adopted": true },
            "claudeSession": { "id": "s-1", "cwd": mine, "permissionMode": "default" },
        });
        save_job(&dir, &job(json!({ "data": data }))).unwrap();
        let refused = load_jobs(&dir, &opts(2000.0, Some(&wt)));
        assert_eq!(refused.dropped, [ID], "dropped by default, as any worktree outside the root");

        save_job(&dir, &job(json!({ "data": data }))).unwrap();
        let seen = Mutex::new(vec![]);
        let vouch = |r: &str, d: &str| {
            seen.lock().unwrap().push((r.to_string(), d.to_string()));
            true
        };
        let ok = load_jobs(&dir, &LoadOptions { now: 2000.0, worktree_root: Some(&wt), adopted_worktree_ok: &vouch, ..Default::default() });
        assert_eq!(*seen.lock().unwrap(), [("/r/app".to_string(), mine.clone())]);
        assert_eq!(ok.jobs.len(), 1);

        // `adopted` alone is not enough for a job file that doesn't say which repo.
        let mut no_repo = data.clone();
        no_repo.as_object_mut().unwrap().remove("repoPath");
        save_job(&dir, &job(json!({ "data": no_repo }))).unwrap();
        let yes = |_: &str, _: &str| true;
        let r = load_jobs(&dir, &LoadOptions { now: 2000.0, worktree_root: Some(&wt), adopted_worktree_ok: &yes, ..Default::default() });
        assert_eq!(r.dropped, [ID]);
    }

    #[test]
    fn is_strictly_inside_keeps_the_node_quirks() {
        assert!(is_strictly_inside(Some("/a/b"), Some("/a/b/c")));
        assert!(is_strictly_inside(Some("/a/b/"), Some("/a/b/c/d/../e")));
        assert!(is_strictly_inside(Some("/"), Some("/x")));
        assert!(!is_strictly_inside(Some("/"), Some("/")));
        assert!(!is_strictly_inside(Some("/a/b"), Some("/a/b")));
        assert!(!is_strictly_inside(Some("/a/b"), Some("/a/bc")));
        assert!(!is_strictly_inside(Some("/a/b"), Some("/a/b/../c")));
        assert!(!is_strictly_inside(Some("/a/b"), Some("/a/b/..hidden")), "a child named ..x counts as outside, as in Node");
        assert!(!is_strictly_inside(None, Some("/a")));
        assert!(!is_strictly_inside(Some("/a"), None));
    }

    #[test]
    fn staleness_and_the_persisted_feature_list() {
        let stale = |status: &str, updated: Value| is_stale(&job(json!({ "status": status, "updatedAt": updated })), 1_000_000.0, 100.0);
        assert!(stale("awaiting-approval", json!(0)));
        assert!(stale("failed", json!(0)));
        assert!(!stale("running", json!(0)));
        assert!(!stale("failed", json!(999_950)));
        assert!(!stale("failed", json!("0")));
        assert_eq!(feature_ids_to_persist(&["a", "digest", "b"], &["digest"]), ["a", "b"]);
    }
}
