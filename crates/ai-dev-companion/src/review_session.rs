//! Live status of a terminal-based review (Claude Code / cursor-agent) after it
//! has been handed to the terminal (port of `core/review-session.js`).
//!
//! The sourced review script (`terminal::build_terminal_script`) appends lines to
//! a per-review status file:
//!   shell <pid> <start time>   the user's interactive shell ($$)
//!   agent <pid> <start time>   a small `sh` the agent runs under, which records
//!                              itself before starting the agent
//!   exit <code>                once the agent has exited
//! and `stop_session` appends `cancel`. Start times are `ps -o lstart=` under
//! LC_ALL=C on both sides, so a recycled PID never matches a record.
//!
//! Next to it, a per-worktree session record (JSON) says which job's review is
//! using that worktree, with what the panel needs to show it and where its
//! terminal is, so a restarted service can still find it. The record never names
//! a path: the status file is derived from its job id.

use crate::app_identity::app_slug;
use crate::ids::uuid_v4;
use crate::node_path;
use crate::terminal_focus::is_valid_terminal_location;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::process::Command;

const STALE_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
/// How long the terminal gets to start sourcing the script before a review with no shell record
/// counts as never having started.
pub const START_TIMEOUT_MS: f64 = 2.0 * 60.0 * 1000.0;

static JOB_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[0-9a-f-]{8,64}$").unwrap());
static PROC_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(shell|agent) ([0-9]+) (.+)$").unwrap());
static EXIT_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^exit [0-9]+$").unwrap());
static PS_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*([0-9]+)\s+([0-9]+)\s+(.+?)\s*$").unwrap());
static RECORD_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^worktree-[0-9a-f]{24}\.json$").unwrap());

pub fn state_dir_name() -> String {
    format!("{}-reviews", app_slug())
}

/// The private directory status files live in, created 0700. Refuses a path that isn't a real
/// directory owned by this user.
pub fn state_dir(base: &Path) -> Result<PathBuf, String> {
    let dir = base.join(state_dir_name());
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    let st = fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    // SAFETY: getuid has no preconditions.
    if !st.is_dir() || st.file_type().is_symlink() || st.uid() != unsafe { libc::getuid() } {
        return Err(format!("{} isn't a directory owned by this user; refusing to track reviews there.", dir.display()));
    }
    if st.mode() & 0o077 != 0 {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

pub fn status_file_for(job_id: &str, dir: &Path) -> Result<PathBuf, String> {
    if !JOB_ID.is_match(job_id) {
        return Err(format!("Invalid job id: {job_id}"));
    }
    Ok(dir.join(format!("{job_id}.status")))
}

/// Creates an empty 0600 status file for `job_id`, first deleting any left over for more than a
/// day, and returns its path.
pub fn create_status_file(job_id: &str, dir: &Path, now_ms: f64) -> Result<PathBuf, String> {
    prune_status_files(dir, now_ms);
    let file = status_file_for(job_id, dir)?;
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file).map_err(|e| e.to_string())?;
    Ok(file)
}

pub fn prune_status_files(dir: &Path, now_ms: f64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".status") && !name.ends_with(".json") {
            continue;
        }
        let mtime_ms = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as f64);
        if mtime_ms.is_some_and(|m| now_ms - m > STALE_MS) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub started: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusRecord {
    pub shell: Option<Proc>,
    pub agent: Option<Proc>,
    pub exit_code: Option<i64>,
    pub cancelled: bool,
}

pub fn parse_status_file(text: &str) -> StatusRecord {
    let mut record = StatusRecord::default();
    for line in text.split('\n') {
        if let Some(c) = PROC_LINE.captures(line) {
            let Ok(pid) = c[2].parse() else { continue };
            let p = Proc { pid, started: c[3].trim().to_string() };
            if &c[1] == "shell" {
                record.shell = Some(p);
            } else {
                record.agent = Some(p);
            }
        } else if EXIT_LINE.is_match(line) {
            record.exit_code = line[5..].parse().ok();
        } else if line == "cancel" {
            record.cancelled = true;
        }
    }
    record
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// "starting" | "running" | "finished" | "cancelled" | "ended".
    pub state: &'static str,
    pub exit_code: Option<i64>,
}

impl Status {
    fn of(state: &'static str) -> Self {
        Self { state, exit_code: None }
    }

    pub fn to_json(&self) -> Value {
        match self.exit_code {
            Some(c) => json!({ "state": self.state, "exitCode": c }),
            None => json!({ "state": self.state }),
        }
    }
}

/// The state from a parsed status file and whether its recorded processes are still the same live
/// processes:
///   starting   the terminal hasn't reached the agent yet
///   running    the agent is running
///   finished   the agent exited on its own (exit code)
///   cancelled  `stop_session` stopped it
///   ended      the shell went away without the agent exiting first, i.e. the terminal was
///              closed (or never started the review)
pub fn compute_status(record: &StatusRecord, shell_alive: bool, agent_alive: bool, age_ms: f64) -> Status {
    if let Some(code) = record.exit_code {
        return Status { state: if record.cancelled { "cancelled" } else { "finished" }, exit_code: Some(code) };
    }
    if record.shell.is_none() {
        return Status::of(if age_ms > START_TIMEOUT_MS { "ended" } else { "starting" });
    }
    if !shell_alive {
        return Status::of(if record.cancelled { "cancelled" } else { "ended" });
    }
    if record.cancelled && !agent_alive {
        return Status::of("cancelled");
    }
    Status::of(if record.agent.is_some() { "running" } else { "starting" })
}

pub fn is_live(status: &Status) -> bool {
    matches!(status.state, "starting" | "running")
}

/// Every descendant of `root_pid`, with start times, in a `ps -A -o pid=,ppid=,lstart=` listing.
pub fn descendants_of(root_pid: u32, ps_output: &str) -> Vec<Proc> {
    let mut children: Vec<(u32, Proc)> = vec![];
    for line in ps_output.split('\n') {
        let Some(m) = PS_LINE.captures(line) else { continue };
        let (Ok(pid), Ok(ppid)) = (m[1].parse::<u32>(), m[2].parse::<u32>()) else { continue };
        children.push((ppid, Proc { pid, started: m[3].to_string() }));
    }
    let mut found: Vec<Proc> = vec![];
    let mut queue = std::collections::VecDeque::from([root_pid]);
    while let Some(parent) = queue.pop_front() {
        for (ppid, child) in &children {
            if *ppid != parent || child.pid == root_pid || found.iter().any(|p| p.pid == child.pid) {
                continue;
            }
            found.push(child.clone());
            queue.push_back(child.pid);
        }
    }
    found
}

async fn ps(args: &[&str]) -> String {
    match Command::new("ps").args(args).env("LC_ALL", "C").output().await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}

/// Whether `proc` is still that same process.
pub async fn is_same_process(proc: Option<&Proc>) -> bool {
    let Some(p) = proc else { return false };
    ps(&["-o", "lstart=", "-p", &p.pid.to_string()]).await.trim() == p.started
}

pub async fn read_status(file: &Path, now_ms: f64) -> Status {
    let Ok(text) = fs::read_to_string(file) else { return Status::of("ended") };
    let created_ms = fs::metadata(file).ok().and_then(|m| m.created().or_else(|_| m.modified()).ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(now_ms, |d| d.as_millis() as f64);
    let record = parse_status_file(&text);
    let (shell_alive, agent_alive) = tokio::join!(is_same_process(record.shell.as_ref()), is_same_process(record.agent.as_ref()));
    compute_status(&record, shell_alive, agent_alive, now_ms - created_ms)
}

async fn still_running(procs: &[Proc]) -> Vec<Proc> {
    let mut live = vec![];
    for p in procs {
        if is_same_process(Some(p)).await {
            live.push(p.clone());
        }
    }
    live
}

async fn signal_all(procs: &[Proc], signal: i32) {
    for p in still_running(procs).await {
        // SAFETY: signalling a process we just confirmed is the recorded one.
        unsafe { libc::kill(p.pid as i32, signal) };
    }
}

async fn wait_for_exit(procs: &[Proc], timeout: Duration) -> bool {
    let mut waited = Duration::ZERO;
    while waited < timeout {
        if still_running(procs).await.is_empty() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        waited += Duration::from_millis(200);
    }
    still_running(procs).await.is_empty()
}

/// Stops a running review: SIGINT to the agent and everything below it, then SIGTERM after
/// `grace` to whatever is left. The process tree is taken (with start times) up front, after
/// checking the recorded wrapper is still that process and still a child of the recorded shell,
/// because the agent's own children are reparented once it exits. Neither the shell nor the
/// wrapper is signalled, so the script carries on and leaves the terminal at a prompt in the
/// worktree.
pub async fn stop_session(file: &Path, grace: Duration) -> Result<(), String> {
    let record = parse_status_file(&fs::read_to_string(file).map_err(|e| e.to_string())?);
    if record.exit_code.is_some() {
        return Ok(());
    }
    let (Some(shell), Some(agent)) = (record.shell.clone(), record.agent.clone()) else { return Err("The review isn't running any more.".into()) };
    if !is_same_process(Some(&shell)).await || !is_same_process(Some(&agent)).await {
        return Err("The review isn't running any more.".into());
    }
    if ps(&["-o", "ppid=", "-p", &agent.pid.to_string()]).await.trim().parse::<u32>().ok() != Some(shell.pid) {
        return Err("The review's agent is no longer attached to its terminal; not stopping it.".into());
    }
    let tree = descendants_of(agent.pid, &ps(&["-A", "-o", "pid=,ppid=,lstart="]).await);
    let mut f = fs::OpenOptions::new().append(true).open(file).map_err(|e| e.to_string())?;
    f.write_all(b"cancel\n").map_err(|e| e.to_string())?;
    drop(f);
    signal_all(&tree, libc::SIGINT).await;
    if wait_for_exit(&tree, grace).await {
        return Ok(());
    }
    signal_all(&tree, libc::SIGTERM).await;
    if !wait_for_exit(&tree, grace).await {
        return Err("The review's agent didn't stop; close its terminal window to end it.".into());
    }
    Ok(())
}

/// Polls until the review in `file` is no longer live, for at most `timeout`; its last status.
pub async fn wait_until_stopped(file: &Path, timeout: Duration, interval: Duration) -> Status {
    let mut status = read_status(file, crate::job_files::now_ms()).await;
    let mut waited = Duration::ZERO;
    while is_live(&status) && waited < timeout {
        tokio::time::sleep(interval).await;
        waited += interval;
        status = read_status(file, crate::job_files::now_ms()).await;
    }
    status
}

pub fn session_record_file(worktree: &str, dir: &Path) -> PathBuf {
    let hash: String = Sha256::digest(node_path::resolve(worktree).as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("worktree-{}.json", &hash[..24]))
}

/// Saves `record` (`{jobId, worktree, payload, summary, review, terminal?}`) as its worktree's
/// session, replacing any earlier one.
pub fn write_session_record(record: &Value, dir: &Path) -> Result<(), String> {
    status_file_for(record["jobId"].as_str().unwrap_or(""), dir)?;
    let worktree = node_path::resolve(record["worktree"].as_str().unwrap_or(""));
    let file = session_record_file(&worktree, dir);
    let mut stored = record.clone();
    stored["worktree"] = json!(worktree);
    let tmp = PathBuf::from(format!("{}.{}.tmp", file.display(), uuid_v4()));
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(stored.to_string().as_bytes()).map_err(|e| e.to_string())?;
    drop(f);
    fs::rename(&tmp, &file).map_err(|e| e.to_string())
}

/// A session record's fields, or `None` unless it's well-formed and for `worktree`. An invalid
/// terminal location is dropped.
pub fn parse_session_record(text: &str, worktree: &str) -> Option<Value> {
    let r: Value = serde_json::from_str(text).ok()?;
    if !r.is_object() || !r["jobId"].as_str().is_some_and(|j| JOB_ID.is_match(j)) || r["worktree"].as_str() != Some(node_path::resolve(worktree).as_str()) {
        return None;
    }
    if !r["payload"].is_object() || !r["review"].is_object() {
        return None;
    }
    let mut out = json!({ "jobId": r["jobId"], "worktree": r["worktree"], "payload": r["payload"], "summary": r["summary"].as_str().unwrap_or(""), "review": r["review"] });
    if is_valid_terminal_location(&r["terminal"]) {
        out["terminal"] = r["terminal"].clone();
    }
    Some(out)
}

pub fn read_session_record(worktree: &str, dir: &Path) -> Option<Value> {
    parse_session_record(&fs::read_to_string(session_record_file(worktree, dir)).ok()?, worktree)
}

/// Every well-formed session record in `dir`.
pub fn list_session_records(dir: &Path) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(dir) else { return vec![] };
    let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| RECORD_NAME.is_match(n)).collect();
    names.sort();
    let mut records = vec![];
    for name in names {
        let Some(worktree) = fs::read_to_string(dir.join(&name)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["worktree"].as_str().map(String::from)) else { continue };
        if session_record_file(&worktree, dir).file_name().is_some_and(|n| n.to_string_lossy() != name) {
            continue;
        }
        if let Some(r) = read_session_record(&worktree, dir) {
            records.push(r);
        }
    }
    records
}

/// Deletes `worktree`'s session record if it's still `job_id`'s.
pub fn remove_session_record(worktree: &str, job_id: &str, dir: &Path) {
    if read_session_record(worktree, dir).is_some_and(|r| r["jobId"] == job_id) {
        let _ = fs::remove_file(session_record_file(worktree, dir));
    }
}

/// `worktree`'s review if it's still live: `(record, status)`.
pub async fn find_live_session(worktree: &str, dir: &Path) -> Option<(Value, Status)> {
    let record = read_session_record(worktree, dir)?;
    let status = read_status(&status_file_for(record["jobId"].as_str()?, dir).ok()?, crate::job_files::now_ms()).await;
    is_live(&status).then_some((record, status))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const STARTED: &str = "Thu Sep 24 16:19:34 2026";
    const JOB: &str = "0a1b2c3d-0000-4000-8000-000000000001";

    fn record(lines: &[&str]) -> StatusRecord {
        parse_status_file(&(lines.join("\n") + "\n"))
    }
    fn p(pid: u32) -> Proc {
        Proc { pid, started: STARTED.into() }
    }
    fn status(state: &'static str, exit_code: Option<i64>) -> Status {
        Status { state, exit_code }
    }
    fn lines(shell: bool, agent: bool, rest: &[&str]) -> StatusRecord {
        let mut v: Vec<String> = vec![];
        if shell {
            v.push(format!("shell 1 {STARTED}"));
        }
        if agent {
            v.push(format!("agent 2 {STARTED}"));
        }
        v.extend(rest.iter().map(|s| s.to_string()));
        record(&v.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn parse_status_file_reads_the_shell_agent_exit_code_and_cancel_lines_and_ignores_the_rest() {
        let r = record(&[&format!("shell 101 {STARTED}"), &format!("agent 202 {STARTED}"), "exit 130", "cancel"]);
        assert_eq!(r, StatusRecord { shell: Some(Proc { pid: 101, started: STARTED.into() }), agent: Some(Proc { pid: 202, started: STARTED.into() }), exit_code: Some(130), cancelled: true });
        assert_eq!(record(&["", "shell x y", "exit", "cancelled", "agent 5"]), StatusRecord::default());
    }

    #[test]
    fn compute_status_covers_every_state() {
        let none = StatusRecord::default();
        assert_eq!(compute_status(&none, false, false, 1000.0), status("starting", None));
        assert_eq!(compute_status(&none, false, false, START_TIMEOUT_MS + 1.0), status("ended", None));
        assert_eq!(compute_status(&lines(true, false, &[]), true, false, 0.0), status("starting", None));
        let running = lines(true, true, &[]);
        assert_eq!(compute_status(&running, true, true, 0.0), status("running", None));
        assert_eq!(compute_status(&lines(true, true, &["exit 0"]), true, false, 0.0), status("finished", Some(0)));
        assert_eq!(compute_status(&running, false, false, 0.0), status("ended", None));
        assert_eq!(compute_status(&running, false, true, 0.0), status("ended", None));
        let cancel = lines(true, true, &["cancel"]);
        assert_eq!(compute_status(&lines(true, true, &["cancel", "exit 130"]), true, false, 0.0), status("cancelled", Some(130)));
        assert_eq!(compute_status(&cancel, true, false, 0.0), status("cancelled", None));
        assert_eq!(compute_status(&cancel, false, false, 0.0), status("cancelled", None));
        assert_eq!(compute_status(&cancel, true, true, 0.0), status("running", None), "a cancel that hasn't taken effect yet");
        assert_eq!(compute_status(&lines(true, true, &["exit 3"]), true, false, 0.0).to_json(), json!({ "state": "finished", "exitCode": 3 }));
    }

    #[test]
    fn is_live_is_true_only_for_starting_and_running() {
        assert!(is_live(&status("starting", None)) && is_live(&status("running", None)));
        for s in ["finished", "cancelled", "ended"] {
            assert!(!is_live(&status(s, None)));
        }
    }

    #[test]
    fn descendants_of_walks_the_whole_process_tree_below_a_pid_with_start_times_and_nothing_else() {
        let ps_out: String = [(1, 0), (10, 1), (20, 10), (21, 10), (30, 20), (40, 1), (50, 40)].iter().map(|(pid, ppid)| format!("  {pid}     {ppid} {STARTED}\n")).collect();
        let mut found = descendants_of(10, &ps_out);
        found.sort_by_key(|p| p.pid);
        assert_eq!(found.iter().map(|p| p.pid).collect::<Vec<_>>(), [20, 21, 30]);
        assert!(found.iter().all(|p| p.started == STARTED));
        assert!(descendants_of(30, &ps_out).is_empty());
        let _ = p(1);
    }

    #[test]
    fn state_dir_is_private_tightened_and_refuses_a_symlink_and_status_files_are_0600() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let file = create_status_file("0a1b2c3d-0000-4000-8000-000000000000", &dir, 0.0).unwrap();
        assert_eq!(file.parent().unwrap(), dir);
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&file).unwrap(), "");

        let loose = tempfile::tempdir().unwrap();
        fs::DirBuilder::new().mode(0o755).create(loose.path().join(state_dir_name())).unwrap();
        fs::set_permissions(loose.path().join(state_dir_name()), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(fs::metadata(state_dir(loose.path()).unwrap()).unwrap().permissions().mode() & 0o777, 0o700);

        let linked = tempfile::tempdir().unwrap();
        fs::create_dir(linked.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(linked.path().join("elsewhere"), linked.path().join(state_dir_name())).unwrap();
        assert!(state_dir(linked.path()).unwrap_err().contains("refusing"));
    }

    #[test]
    fn create_status_file_rejects_a_job_id_that_isnt_a_plain_id_and_prunes_day_old_files() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        assert!(create_status_file("../../etc/passwd", &dir, 0.0).unwrap_err().starts_with("Invalid job id"));
        let old = dir.join("deadbeef.status");
        fs::write(&old, "").unwrap();
        let now = crate::job_files::now_ms();
        create_status_file("cafebabe", &dir, now + 25.0 * 60.0 * 60.0 * 1000.0).unwrap();
        assert!(!old.exists());
        assert!(dir.join("cafebabe.status").exists(), "the new file is never pruned");
    }

    fn session_record(worktree: &str, over: Value) -> Value {
        let mut r = json!({
            "jobId": JOB, "worktree": worktree,
            "payload": { "project": "ACME", "repo": "sample-app", "prId": 5429, "sourceBranch": "feature" },
            "summary": "Checked out feature.", "review": { "prId": 5429, "tracked": true },
            "terminal": { "app": "iterm", "windowId": "7", "sessionId": "0F3B5B28-6C1D-4E0A-9E2B-5A1C2D3E4F50" },
        });
        for (k, v) in over.as_object().into_iter().flatten() {
            r[k] = v.clone();
        }
        r
    }

    #[test]
    fn session_records_are_0600_keyed_by_worktree_and_read_back_as_written() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        write_session_record(&session_record("/wt/a/", json!({})), &dir).unwrap();
        let file = session_record_file("/wt/a", &dir);
        assert_eq!(file.parent().unwrap(), dir);
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_session_record("/wt/a", &dir), Some(session_record("/wt/a", json!({}))));
        assert_eq!(read_session_record("/wt/b", &dir), None);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(file.file_name().unwrap().to_string_lossy().len(), "worktree-".len() + 24 + ".json".len());
    }

    #[test]
    fn write_session_record_replaces_the_worktrees_earlier_record_and_rejects_a_bad_job_id() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        write_session_record(&session_record("/wt/a", json!({})), &dir).unwrap();
        let next = "0a1b2c3d-0000-4000-8000-000000000002";
        write_session_record(&session_record("/wt/a", json!({ "jobId": next })), &dir).unwrap();
        assert_eq!(read_session_record("/wt/a", &dir).unwrap()["jobId"], next);
        assert!(write_session_record(&session_record("/wt/a", json!({ "jobId": "../x" })), &dir).unwrap_err().starts_with("Invalid job id"));
    }

    #[test]
    fn parse_session_record_refuses_malformed_records_and_drops_an_invalid_terminal() {
        let good = session_record("/wt/a", json!({}));
        assert_eq!(parse_session_record("not json", "/wt/a"), None);
        assert_eq!(parse_session_record(&good.to_string(), "/wt/other"), None);
        assert_eq!(parse_session_record(&session_record("/wt/a", json!({ "jobId": "../../x" })).to_string(), "/wt/a"), None);
        assert_eq!(parse_session_record(&session_record("/wt/a", json!({ "payload": null })).to_string(), "/wt/a"), None);
        let bad_terminal = json!({ "app": "terminal", "windowId": "1", "tty": "/dev/ttys1\" & quit" });
        assert!(parse_session_record(&session_record("/wt/a", json!({ "terminal": bad_terminal })).to_string(), "/wt/a").unwrap().get("terminal").is_none());
    }

    #[test]
    fn list_session_records_returns_only_well_formed_records_under_their_own_names() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        write_session_record(&session_record("/wt/a", json!({})), &dir).unwrap();
        fs::write(dir.join("worktree-000000000000000000000000.json"), session_record("/wt/b", json!({})).to_string()).unwrap();
        fs::write(dir.join("worktree-111111111111111111111111.json"), "{").unwrap();
        let listed: Vec<String> = list_session_records(&dir).iter().map(|r| r["worktree"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, ["/wt/a"]);
    }

    #[test]
    fn remove_session_record_only_removes_the_record_of_the_given_job() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        write_session_record(&session_record("/wt/a", json!({})), &dir).unwrap();
        remove_session_record("/wt/a", "0a1b2c3d-0000-4000-8000-00000000ffff", &dir);
        assert!(read_session_record("/wt/a", &dir).is_some());
        remove_session_record("/wt/a", JOB, &dir);
        assert!(read_session_record("/wt/a", &dir).is_none());
    }

    #[tokio::test]
    async fn find_live_session_reports_a_just_created_review_as_live_and_a_missing_one_as_not() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        assert!(find_live_session("/wt/a", &dir).await.is_none());
        write_session_record(&session_record("/wt/a", json!({})), &dir).unwrap();
        assert!(find_live_session("/wt/a", &dir).await.is_none(), "no status file: ended");
        create_status_file(JOB, &dir, crate::job_files::now_ms()).unwrap();
        let (rec, st) = find_live_session("/wt/a", &dir).await.unwrap();
        assert_eq!((st.state, rec["jobId"].clone()), ("starting", json!(JOB)));
        fs::write(status_file_for(JOB, &dir).unwrap(), "exit 0\n").unwrap();
        assert!(find_live_session("/wt/a", &dir).await.is_none());
    }

    #[tokio::test]
    async fn stop_session_signals_the_agent_and_what_it_started_and_notes_the_cancel() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        let file = create_status_file(JOB, &dir, crate::job_files::now_ms()).unwrap();
        // Our own process stands in for the user's shell; a child of it for the agent wrapper.
        let mut child = Command::new("sh").args(["-c", "sleep 30 & wait"]).spawn().unwrap();
        let (shell_pid, agent_pid) = (std::process::id(), child.id().unwrap());
        let started = |pid: u32| async move { ps(&["-o", "lstart=", "-p", &pid.to_string()]).await.trim().to_string() };
        fs::write(&file, format!("shell {shell_pid} {}\nagent {agent_pid} {}\n", started(shell_pid).await, started(agent_pid).await)).unwrap();
        let reaper = tokio::spawn(async move { child.wait().await });
        assert_eq!(read_status(&file, crate::job_files::now_ms()).await.state, "running");
        stop_session(&file, Duration::from_secs(3)).await.unwrap();
        assert!(fs::read_to_string(&file).unwrap().ends_with("cancel\n"));
        let _ = reaper.await;
        let after = wait_until_stopped(&file, Duration::from_secs(3), Duration::from_millis(50)).await;
        assert_eq!(after.state, "cancelled");
    }

    #[tokio::test]
    async fn stop_session_refuses_a_review_that_isnt_running_or_whose_agent_is_not_attached_to_its_shell() {
        let base = tempfile::tempdir().unwrap();
        let dir = state_dir(base.path()).unwrap();
        let file = create_status_file(JOB, &dir, crate::job_files::now_ms()).unwrap();
        assert_eq!(stop_session(&file, Duration::from_millis(50)).await.unwrap_err(), "The review isn't running any more.");
        fs::write(&file, "exit 0\n").unwrap();
        stop_session(&file, Duration::from_millis(50)).await.unwrap();
        // A recorded pid whose start time doesn't match is a recycled pid: never signalled.
        fs::write(&file, format!("shell {0} {STARTED}\nagent {0} {STARTED}\n", std::process::id())).unwrap();
        assert_eq!(stop_session(&file, Duration::from_millis(50)).await.unwrap_err(), "The review isn't running any more.");
        // Both real, but the agent is not a child of the recorded shell.
        let started = ps(&["-o", "lstart=", "-p", "1"]).await.trim().to_string();
        let me = ps(&["-o", "lstart=", "-p", &std::process::id().to_string()]).await.trim().to_string();
        fs::write(&file, format!("shell 1 {started}\nagent {} {me}\n", std::process::id())).unwrap();
        assert_eq!(stop_session(&file, Duration::from_millis(50)).await.unwrap_err(), "The review's agent is no longer attached to its terminal; not stopping it.");
        assert!(!fs::read_to_string(&file).unwrap().contains("cancel"));
    }
}
