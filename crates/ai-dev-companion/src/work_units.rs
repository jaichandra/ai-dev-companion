//! The registry of work units (port of `core/work-units.js`): one record per
//! branch the tool has worked on (repo + branch), with the worktree it used,
//! the ticket and PR it belongs to and every Claude session started there. It
//! is what lets a panel or `companion resume` find the session again, and what
//! a later cleanup command lists. Operational state, not history: it does not
//! depend on the history opt-in and is never expired by its retention. One
//! small JSON file in the state dir, written atomically.

use crate::js_text::{js_trim, slice_utf16};
use crate::node_path;
use crate::paths::state_dir;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const VERSION: i64 = 1;
pub const MAX_UNITS: usize = 500;
pub const MAX_SESSIONS: usize = 20;
const STR_MAX: usize = 500;
static SESSION_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,100}$").unwrap());

/// Control characters become spaces, trimmed, clipped; `None` when empty or not text.
fn clean(v: Option<&str>) -> Option<String> {
    let spaced: String = v?.chars().map(|c| if matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}') { ' ' } else { c }).collect();
    let t = js_trim(&spaced);
    (!t.is_empty()).then(|| slice_utf16(t, STR_MAX))
}

/// `<repo path>#<branch>`: the same branch in two clones is two units.
pub fn unit_key(repo_path: &str, branch: &str) -> Result<String, String> {
    if clean(Some(repo_path)).is_none() || clean(Some(branch)).is_none() {
        return Err("A work unit needs a repo path and a branch.".into());
    }
    Ok(format!("{}#{branch}", node_path::resolve(repo_path)))
}

pub fn default_file(home: &Path) -> PathBuf {
    state_dir(home).join("work-units.json")
}

fn load(file: &Path) -> Map<String, Value> {
    // Missing or unreadable: start empty rather than block the feature.
    let Ok(text) = fs::read(file) else { return Map::new() };
    match serde_json::from_str::<Value>(&String::from_utf8_lossy(&text)) {
        Ok(v) if v["version"] == json!(VERSION) => v["units"].as_object().cloned().unwrap_or_default(),
        _ => Map::new(),
    }
}

fn save(file: &Path, units: &Map<String, Value>) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = format!("{}.{}.tmp", file.display(), std::process::id());
    let text = serde_json::to_string_pretty(&json!({ "version": VERSION, "units": units })).map_err(io::Error::other)?;
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    fs::rename(&tmp, file)
}

#[derive(Default)]
pub struct UnitFields<'a> {
    pub dir: Option<&'a str>,
    pub issue_key: Option<&'a str>,
    pub pr_key: Option<&'a str>,
}

/// Each call reads the file afresh, so two features (or the service and the
/// CLI) see each other's writes.
pub struct WorkUnits {
    file: PathBuf,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl WorkUnits {
    pub fn open(file: PathBuf) -> Self {
        Self { file, now: Box::new(|| crate::job_files::now_ms() as i64) }
    }

    pub fn with_clock(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    fn mutate<R>(&self, f: impl FnOnce(&mut Map<String, Value>) -> R) -> io::Result<R> {
        let mut units = load(&self.file);
        let result = f(&mut units);
        if units.len() > MAX_UNITS {
            let mut keys: Vec<String> = units.keys().cloned().collect();
            let updated = |k: &String| units[k]["updatedAt"].as_f64().unwrap_or(0.0);
            keys.sort_by(|a, b| updated(a).partial_cmp(&updated(b)).unwrap_or(std::cmp::Ordering::Equal));
            let drop_n = keys.len() - MAX_UNITS;
            for k in keys.into_iter().take(drop_n) {
                units.shift_remove(&k);
            }
        }
        save(&self.file, &units)?;
        Ok(result)
    }

    /// Creates the unit or updates the fields given (never blanks one).
    pub fn upsert(&self, repo_path: &str, branch: &str, fields: &UnitFields<'_>) -> Result<Value, String> {
        let key = unit_key(repo_path, branch)?;
        let at = (self.now)();
        self.mutate(|units| {
            let mut unit = units.get(&key).cloned().unwrap_or_else(|| {
                json!({ "key": key, "repoPath": node_path::resolve(repo_path), "branch": branch, "dir": null, "issueKey": null, "prKey": null, "createdAt": at, "updatedAt": at, "sessions": [] })
            });
            for (name, v) in [("dir", fields.dir), ("issueKey", fields.issue_key), ("prKey", fields.pr_key)] {
                if let Some(c) = clean(v) {
                    unit[name] = json!(c);
                }
            }
            unit["updatedAt"] = json!(at);
            units.insert(key.clone(), unit.clone());
            unit
        })
        .map_err(|e| e.to_string())
    }

    /// Records a Claude session; the same id again only refreshes it. `Ok(None)` for an unknown unit.
    pub fn add_session(&self, key: &str, id: &str, cwd: Option<&str>, feature: Option<&str>) -> Result<Option<Value>, String> {
        if !SESSION_ID_RE.is_match(id) {
            return Err("Not a Claude session id.".into());
        }
        let at = (self.now)();
        self.mutate(|units| {
            let unit = units.get_mut(key)?;
            let mut sessions: Vec<Value> = unit["sessions"].as_array().cloned().unwrap_or_default();
            sessions.retain(|s| s["id"] != id);
            sessions.push(json!({ "id": id, "cwd": clean(cwd), "feature": clean(feature), "at": at }));
            if sessions.len() > MAX_SESSIONS {
                sessions.drain(..sessions.len() - MAX_SESSIONS);
            }
            unit["sessions"] = Value::Array(sessions);
            unit["updatedAt"] = json!(at);
            Some(unit.clone())
        })
        .map_err(|e| e.to_string())
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        load(&self.file).get(key).cloned()
    }

    fn find_by(&self, field: &str, value: &str) -> Vec<Value> {
        load(&self.file).into_values().filter(|u| u[field] == value).collect()
    }

    pub fn find_by_issue(&self, issue_key: &str) -> Vec<Value> {
        self.find_by("issueKey", issue_key)
    }

    pub fn find_by_pr(&self, pr_key: &str) -> Vec<Value> {
        self.find_by("prKey", pr_key)
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Value> {
        let mut all: Vec<Value> = load(&self.file).into_values().collect();
        all.sort_by(|a, b| b["updatedAt"].as_f64().partial_cmp(&a["updatedAt"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        all
    }

    pub fn remove(&self, key: &str) -> Result<bool, String> {
        self.mutate(|units| units.shift_remove(key).is_some()).map_err(|e| e.to_string())
    }

    /// Removes the units `is_live` says are gone (their worktree, or with no
    /// dir their branch, no longer exists) and returns them.
    pub fn prune(&self, is_live: impl Fn(&Value) -> bool) -> Result<Vec<Value>, String> {
        self.mutate(|units| {
            let dead: Vec<String> = units.iter().filter(|(_, u)| !is_live(u)).map(|(k, _)| k.clone()).collect();
            dead.iter().filter_map(|k| units.shift_remove(k)).collect()
        })
        .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    struct S {
        _dir: tempfile::TempDir,
        file: PathBuf,
        reg: WorkUnits,
    }

    fn setup() -> S {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state").join("work-units.json");
        let t = Arc::new(AtomicI64::new(1000));
        let reg = WorkUnits::open(file.clone()).with_clock(move || t.fetch_add(1, Ordering::SeqCst) + 1);
        S { _dir: dir, file, reg }
    }

    fn up(s: &S, repo: &str, branch: &str, f: UnitFields<'_>) -> Value {
        s.reg.upsert(repo, branch, &f).unwrap()
    }

    #[test]
    fn upsert_creates_a_unit_then_fills_in_fields_without_blanking_any() {
        let s = setup();
        let a = up(&s, "/r/app", "feature/X-1", UnitFields { dir: Some("/w/x1"), issue_key: Some("X-1"), pr_key: None });
        assert_eq!(a["key"], unit_key("/r/app", "feature/X-1").unwrap().as_str());
        let b = up(&s, "/r/app", "feature/X-1", UnitFields { pr_key: Some("bitbucket:PROJ/app#4"), ..Default::default() });
        assert_eq!((b["dir"].as_str(), b["issueKey"].as_str(), b["prKey"].as_str()), (Some("/w/x1"), Some("X-1"), Some("bitbucket:PROJ/app#4")));
        assert_eq!(b["createdAt"], a["createdAt"]);
        assert!(b["updatedAt"].as_i64() > a["updatedAt"].as_i64());
        assert_eq!(s.reg.list().len(), 1);
        assert_eq!(
            a.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            ["key", "repoPath", "branch", "dir", "issueKey", "prKey", "createdAt", "updatedAt", "sessions"]
        );
    }

    #[test]
    fn units_are_found_by_ticket_and_by_pr_and_survive_a_new_handle_on_the_same_file() {
        let s = setup();
        up(&s, "/r/app", "b1", UnitFields { issue_key: Some("X-1"), pr_key: Some("p#1"), dir: None });
        up(&s, "/r/other", "b2", UnitFields { issue_key: Some("X-1"), ..Default::default() });
        let reopened = WorkUnits::open(s.file.clone());
        assert_eq!(reopened.find_by_issue("X-1").len(), 2);
        assert_eq!(reopened.find_by_pr("p#1").len(), 1);
        assert_eq!(reopened.find_by_pr("nope").len(), 0);
    }

    #[test]
    fn add_session_records_refreshes_and_caps_sessions_and_ignores_an_unknown_unit() {
        let s = setup();
        let key = up(&s, "/r/app", "b1", UnitFields::default())["key"].as_str().unwrap().to_string();
        s.reg.add_session(&key, "s-1", Some("/w/b1"), Some("address-review-comments")).unwrap();
        s.reg.add_session(&key, "s-2", None, None).unwrap();
        let again = s.reg.add_session(&key, "s-1", None, Some("resolve-conflict")).unwrap().unwrap();
        let ids: Vec<&str> = again["sessions"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["s-2", "s-1"]);
        assert_eq!(again["sessions"][1]["feature"], "resolve-conflict");
        assert_eq!(again["sessions"][0]["cwd"], Value::Null);
        for i in 0..MAX_SESSIONS + 5 {
            s.reg.add_session(&key, &format!("n-{i}"), None, None).unwrap();
        }
        assert_eq!(s.reg.get(&key).unwrap()["sessions"].as_array().unwrap().len(), MAX_SESSIONS);
        assert_eq!(s.reg.add_session("/r/none#x", "s-9", None, None).unwrap(), None);
        assert!(s.reg.add_session(&key, "bad id; rm", None, None).unwrap_err().contains("session id"));
    }

    #[test]
    fn remove_and_prune_drop_units_prune_keeps_the_live_ones() {
        let s = setup();
        let a = up(&s, "/r/app", "a", UnitFields { dir: Some("/gone"), ..Default::default() });
        up(&s, "/r/app", "b", UnitFields { dir: Some("/here"), ..Default::default() });
        let removed = s.reg.prune(|u| u["dir"] == "/here").unwrap();
        assert_eq!(removed.iter().map(|u| u["branch"].as_str().unwrap()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(s.reg.get(a["key"].as_str().unwrap()), None);
        assert!(s.reg.remove(&unit_key("/r/app", "b").unwrap()).unwrap());
        assert!(!s.reg.remove(&unit_key("/r/app", "b").unwrap()).unwrap());
    }

    #[test]
    fn a_corrupt_or_foreign_file_reads_as_empty_and_is_rewritten_on_the_next_change() {
        let s = setup();
        fs::create_dir_all(s.file.parent().unwrap()).unwrap();
        fs::write(&s.file, "{ not json").unwrap();
        assert!(s.reg.list().is_empty());
        up(&s, "/r/app", "a", UnitFields::default());
        assert_eq!(serde_json::from_str::<Value>(&fs::read_to_string(&s.file).unwrap()).unwrap()["version"], 1);
        fs::write(&s.file, json!({ "version": 99, "units": { "x": {} } }).to_string()).unwrap();
        assert!(s.reg.list().is_empty());
    }

    #[test]
    fn the_file_is_private_and_no_temp_file_is_left_behind() {
        let s = setup();
        up(&s, "/r/app", "a", UnitFields::default());
        assert_eq!(fs::metadata(&s.file).unwrap().permissions().mode() & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(s.file.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["work-units.json"]);
    }

    #[test]
    fn upsert_needs_a_repo_and_a_branch_and_cleans_the_text_it_stores() {
        let s = setup();
        assert!(s.reg.upsert("/r/app", "", &UnitFields::default()).unwrap_err().contains("repo path and a branch"));
        assert!(s.reg.upsert("  ", "b", &UnitFields::default()).is_err());
        let u = up(&s, "/r/app", "b", UnitFields { dir: Some(" /w/\tx \n"), issue_key: Some("   "), pr_key: None });
        assert_eq!(u["dir"], "/w/ x");
        assert_eq!(u["issueKey"], Value::Null, "a blank value never fills a field");
    }

    #[test]
    fn the_registry_keeps_at_most_500_units_dropping_the_least_recently_updated() {
        let s = setup();
        for i in 0..MAX_UNITS + 3 {
            up(&s, "/r/app", &format!("b{i}"), UnitFields::default());
        }
        let all = s.reg.list();
        assert_eq!(all.len(), MAX_UNITS);
        assert!(s.reg.get(&unit_key("/r/app", "b0").unwrap()).is_none() && s.reg.get(&unit_key("/r/app", "b502").unwrap()).is_some());
        assert_eq!(all[0]["branch"], "b502", "list is newest first");
    }
}
