//! Versioning and self-update (port of `core/updater.js`), shared by the service's `GET /update`
//! and `POST /update/apply`, the detached update runner (`update_runner`) and the installer.
//!
//! The install source is a git remote + branch (`install-source.json`, written by the
//! installer). Checking keeps a shallow clone of it under the state folder and reads the version
//! from its `package.json`, so it uses the user's existing git credentials and needs no browser
//! session.
//!
//! Nothing here reads a global location: where this copy lives is a [`Locations`] value the caller
//! builds once, and `home` is explicit, so tests point everything at a temp dir.

use crate::paths;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// A copy installed from `sourceDir` follows `{ url, ref }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub url: String,
    pub git_ref: String,
}

impl Source {
    pub fn to_json(&self) -> Value {
        json!({ "url": self.url, "ref": self.git_ref })
    }
}

/// Installs follow the profile's ref, not master: pushing to master ships nothing until `release`
/// is moved.
pub fn default_source() -> Source {
    let section = crate::environment::current().section("updateSource");
    let text = |k: &str| section.and_then(|s| s.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    Source { url: text("url"), git_ref: text("ref") }
}

pub fn read_json(file: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

/// Where this copy of the service lives.
#[derive(Debug, Clone)]
pub struct Locations {
    /// `companion-service/`: the folder holding the service's `package.json` and `install-source.json`.
    pub companion_service_dir: PathBuf,
    pub home: PathBuf,
}

/// The package name `companion-service/package.json` carries.
const SERVICE_PACKAGE_NAME: &str = "bitbucket-ai-companion";

/// `companion-service/`, found by walking up from `start` (the service loads from its compiled
/// copy in `dist/core/`, setup and tests from `core/`).
pub fn find_companion_service_dir(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    for _ in 0..4 {
        // A `package.json` copied into dist/ is not the service dir.
        if dir.file_name().is_none_or(|n| n != "dist") && read_json(&dir.join("package.json")).is_some_and(|p| p["name"] == SERVICE_PACKAGE_NAME) {
            return dir;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    start.join("..")
}

impl Locations {
    pub fn install_dir(&self) -> PathBuf {
        self.companion_service_dir.join("..")
    }
    pub fn install_source_path(&self) -> PathBuf {
        self.companion_service_dir.join("install-source.json")
    }
    pub fn read_install_source(&self) -> Source {
        read_install_source(&self.install_source_path())
    }
}

pub fn source_cache_dir(home: &Path) -> PathBuf {
    paths::state_dir(home).join("update-source")
}

/// A separate clone for applying an update, so the periodic check (which syncs
/// `source_cache_dir`) can never rewrite it mid-install.
pub fn staging_dir(home: &Path) -> PathBuf {
    paths::state_dir(home).join("update-staging")
}

/// `preferred` if it exists, else `fallback` if THAT exists, else `preferred` anyway (so a first
/// write still lands in the new dir).
fn existing_or(preferred: PathBuf, fallback: PathBuf) -> PathBuf {
    if preferred.exists() {
        preferred
    } else if fallback.exists() {
        fallback
    } else {
        preferred
    }
}

/// `update-status.json` and `update.log` fall back to the legacy state dir when the new one doesn't
/// have them yet, for READS only: a runner started before a migration may have left its status
/// file there. Writes always go to `new_status_path`/`new_log_path`, never here: a sticky
/// fallback would keep writing into the legacy dir forever.
pub fn status_path(home: &Path) -> PathBuf {
    existing_or(paths::state_dir(home).join("update-status.json"), paths::legacy_state_dir(home).join("update-status.json"))
}

pub fn log_path(home: &Path) -> PathBuf {
    existing_or(paths::state_dir(home).join("update.log"), paths::legacy_state_dir(home).join("update.log"))
}

pub fn new_status_path(home: &Path) -> PathBuf {
    paths::state_dir(home).join("update-status.json")
}

pub fn new_log_path(home: &Path) -> PathBuf {
    paths::state_dir(home).join("update.log")
}

/// Where the installer always installs to; only a copy running from here can update itself (a dev
/// checkout can't).
pub fn stable_install_dir(home: &Path) -> PathBuf {
    home.join(crate::app_identity::app_slug())
}

/// `parseInt(n, 10) || 0`: the leading digits, else 0.
fn parse_int(part: &str) -> u64 {
    let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

fn version_parts(v: &str) -> Vec<u64> {
    let v = crate::js_text::js_trim(v);
    let v = v.strip_prefix('v').unwrap_or(v);
    let core = v.split(['-', '+']).next().unwrap_or("");
    core.split('.').map(parse_int).collect()
}

/// "0.10.2" > "0.9.9". Missing parts count as 0; anything after "-" or "+" is ignored. -1, 0 or 1.
pub fn compare_versions(a: &str, b: &str) -> i32 {
    let (pa, pb) = (version_parts(a), version_parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        if x != y {
            return if x > y { 1 } else { -1 };
        }
    }
    0
}

static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").unwrap());
static SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static RELEASE_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^##\s+v?(\d+(?:\.\d+)*)\b").unwrap());
static ANY_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{1,2}\s").unwrap());
static BULLET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-*]\s+").unwrap());
static CONTINUATION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+\S").unwrap());

/// Markdown inline formatting reduced to plain text: the extension renders notes with textContent.
fn plain_text(markdown: &str) -> String {
    let linked = LINK_RE.replace_all(markdown, "$1");
    let bare = linked.replace("**", "").replace("__", "").replace('`', "");
    crate::js_text::js_trim(&SPACE_RE.replace_all(&bare, " ")).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub version: String,
    pub notes: Vec<String>,
}

impl ChangelogEntry {
    pub fn to_json(&self) -> Value {
        json!({ "version": self.version, "notes": self.notes })
    }
}

/// CHANGELOG.md's release sections in file order. A section starts at `## <version>` (anything
/// after the version is ignored); its notes are its `- ` bullets, with indented continuation lines
/// joined on. Other sections ("Unreleased") and text are skipped.
pub fn parse_changelog(text: &str) -> Vec<ChangelogEntry> {
    let mut entries: Vec<ChangelogEntry> = vec![];
    let mut in_entry = false;
    let mut note: Option<String> = None;
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        let heading = RELEASE_HEADING_RE.captures(line);
        if heading.is_some() || ANY_HEADING_RE.is_match(line) {
            in_entry = heading.is_some();
            if let Some(h) = heading {
                entries.push(ChangelogEntry { version: h[1].to_string(), notes: vec![] });
            }
            note = None;
        } else if !in_entry {
            continue;
        } else if BULLET_RE.is_match(line) {
            let n = BULLET_RE.replace(line, "").into_owned();
            entries.last_mut().expect("in an entry").notes.push(n.clone());
            note = Some(n);
        } else if let (Some(prev), true) = (note.as_ref(), CONTINUATION_RE.is_match(line)) {
            let joined = format!("{prev} {}", crate::js_text::js_trim(line));
            *entries.last_mut().expect("in an entry").notes.last_mut().expect("a note") = joined.clone();
            note = Some(joined);
        } else {
            note = None;
        }
    }
    entries
        .into_iter()
        .map(|e| ChangelogEntry { version: e.version, notes: e.notes.iter().map(|n| plain_text(n)).filter(|n| !n.is_empty()).collect() })
        .filter(|e| !e.notes.is_empty())
        .collect()
}

pub fn read_changelog(dir: &Path) -> Vec<ChangelogEntry> {
    std::fs::read_to_string(dir.join("CHANGELOG.md")).map(|t| parse_changelog(&t)).unwrap_or_default()
}

/// This copy's own CHANGELOG.md, newest version first: what Settings shows under About, always (the
/// update panel's What's new only exists while an update is on offer).
pub fn installed_changelog(install_dir: &Path) -> Vec<ChangelogEntry> {
    let mut entries = read_changelog(install_dir);
    entries.sort_by(|a, b| compare_versions(&b.version, &a.version).cmp(&0));
    entries
}

/// The entries newer than `current` and no newer than `latest`, newest first.
pub fn whats_new_since(entries: &[ChangelogEntry], current: &str, latest: Option<&str>) -> Vec<ChangelogEntry> {
    let mut out: Vec<ChangelogEntry> = entries.iter().filter(|e| compare_versions(&e.version, current) > 0 && latest.is_none_or(|l| l.is_empty() || compare_versions(&e.version, l) <= 0)).cloned().collect();
    out.sort_by(|a, b| compare_versions(&b.version, &a.version).cmp(&0));
    out
}

fn read_package_version(dir: &Path) -> Option<String> {
    read_json(&dir.join("package.json")).and_then(|p| p["version"].as_str().map(String::from))
}

/// This copy's version: the root `package.json`, which a test keeps in lockstep with
/// `companion-service/package.json` and the extension manifest.
pub fn installed_version(install_dir: &Path) -> Option<String> {
    read_package_version(install_dir).or_else(|| read_package_version(&install_dir.join("companion-service")))
}

/// `{ url, ref }` from `install-source.json`'s contents, falling back to the default repo.
pub fn parse_install_source(raw: Option<&Value>) -> Source {
    let default = default_source();
    let Some(raw) = raw.filter(|r| r.is_object()) else { return default };
    let url = raw["url"].as_str().map(crate::js_text::js_trim).filter(|u| !u.is_empty()).map(String::from).unwrap_or(default.url.clone());
    let git_ref = raw["ref"].as_str().filter(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))).map(String::from).unwrap_or(default.git_ref);
    Source { url, git_ref }
}

/// Where a copy installed from `source_dir` should take its updates from: the root
/// `package.json`'s `updateSource` if it declares one, else the clone's git origin, else the
/// default repo. `updateSource` is how the repo moves without anyone reinstalling. `url`/`git_ref`
/// override both (for testing against a local repo).
pub fn resolve_install_source(source_dir: Option<&Path>, origin: Option<&str>, url: Option<&str>, git_ref: Option<&str>) -> Source {
    let declared = source_dir.and_then(|d| read_json(&d.join("package.json"))).map(|p| p["updateSource"].clone());
    let base = match declared.filter(|d| d["url"].as_str().is_some_and(|u| !u.is_empty())) {
        Some(d) => parse_install_source(Some(&d)),
        None => parse_install_source(origin.map(|o| json!({ "url": o })).as_ref()),
    };
    let pick = |over: Option<&str>, fallback: &str| over.filter(|o| !o.is_empty()).unwrap_or(fallback).to_string();
    parse_install_source(Some(&json!({ "url": pick(url, &base.url), "ref": pick(git_ref, &base.git_ref) })))
}

pub fn read_install_source(file: &Path) -> Source {
    parse_install_source(read_json(file).as_ref())
}

static SCP_LIKE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^/]+@[^:]+:").unwrap());

/// What to run by hand if updating from the browser isn't possible.
pub fn manual_update_command(source: &Source) -> String {
    let remote = ["https://", "http://", "ssh://"].iter().any(|p| source.url.starts_with(p));
    if remote || SCP_LIKE_RE.is_match(&source.url) {
        let git_url = if source.url.starts_with("git@") { format!("ssh://{}", source.url.replacen(':', "/", 1)) } else { source.url.clone() };
        return format!("npx --yes \"git+{git_url}#{}\"", source.git_ref);
    }
    format!("cd \"{}\" && git pull && cargo run --release -- install --yes", source.url)
}

async fn run_git(args: &[&str], cwd: &Path) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("GIT_TERMINAL_PROMPT", "0").kill_on_drop(true);
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let out = match tokio::time::timeout(Duration::from_secs(120), cmd.output()).await {
        Ok(out) => out.map_err(|e| e.to_string())?,
        Err(_) => return Err("git timed out".into()),
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let message = if stderr.is_empty() { format!("git exited with {}", out.status) } else { stderr };
    let lines: Vec<&str> = message.trim().split('\n').collect();
    Err(lines[lines.len().saturating_sub(3)..].join("\n"))
}

/// Brings the shallow clone of the source up to date (cloning it the first time, or again if the
/// source URL changed) and returns its directory.
pub async fn sync_source(source: &Source, dir: &Path) -> Result<PathBuf, String> {
    let mut reuse = dir.join(".git").exists();
    if reuse {
        let origin = run_git(&["remote", "get-url", "origin"], dir).await.unwrap_or_default();
        reuse = origin == source.url;
    }
    if reuse {
        run_git(&["fetch", "--depth", "1", "--quiet", "origin", &source.git_ref], dir).await?;
        run_git(&["reset", "--hard", "--quiet", "FETCH_HEAD"], dir).await?;
        run_git(&["clean", "-fdxq"], dir).await?;
    } else {
        let _ = std::fs::remove_dir_all(dir);
        let parent = dir.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        run_git(&["clone", "--depth", "1", "--quiet", "--branch", &source.git_ref, &source.url, &dir.to_string_lossy()], parent).await?;
    }
    Ok(dir.to_path_buf())
}

/// The outcome of the last check for a newer version.
#[derive(Debug, Clone, Default)]
pub struct Check {
    pub latest: Option<String>,
    pub changelog: Vec<ChangelogEntry>,
    pub error: Option<String>,
    pub checked_at: Option<f64>,
}

pub const DEFAULT_CHECK_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// One per service, so every caller shares the last answer and a second caller asking while a
/// check is in flight waits for that one.
pub struct UpdateChecker {
    home: PathBuf,
    last: Mutex<Option<Check>>,
    in_flight: tokio::sync::Mutex<()>,
}

impl UpdateChecker {
    pub fn new(home: PathBuf) -> Arc<Self> {
        Arc::new(Self { home, last: Mutex::new(None), in_flight: tokio::sync::Mutex::new(()) })
    }

    fn fresh(&self, max_age: Duration) -> Option<Check> {
        let last = self.last.lock().unwrap().clone()?;
        (crate::job_files::now_ms() - last.checked_at? < max_age.as_millis() as f64).then_some(last)
    }

    /// Latest version at the install source, cached for `max_age`. Never fails: a failure is
    /// returned as `error` (offline, no credentials).
    pub async fn check_for_update(&self, max_age: Duration, source: &Source) -> Check {
        if let Some(c) = self.fresh(max_age) {
            return c;
        }
        let _guard = self.in_flight.lock().await;
        // Another caller may have finished the same check while this one waited.
        if let Some(c) = self.fresh(max_age) {
            return c;
        }
        let outcome = async {
            let dir = sync_source(source, &source_cache_dir(&self.home)).await?;
            let latest = installed_version(&dir).ok_or_else(|| format!("{} ({}) has no package.json version.", source.url, source.git_ref))?;
            Ok::<_, String>(Check { latest: Some(latest), changelog: read_changelog(&dir), error: None, checked_at: Some(crate::job_files::now_ms()) })
        }
        .await;
        let mut last = self.last.lock().unwrap();
        let check = match outcome {
            Ok(c) => c,
            Err(error) => {
                let previous = last.clone().unwrap_or_default();
                Check { latest: previous.latest, changelog: previous.changelog, error: Some(error), checked_at: Some(crate::job_files::now_ms()) }
            }
        };
        *last = Some(check.clone());
        check
    }

    pub fn last_check(&self) -> Option<Check> {
        self.last.lock().unwrap().clone()
    }
}

fn is_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// The last (or current) update attempt from `update-status.json`, or `None`. A "running" attempt
/// whose runner process is gone is reported as failed.
pub fn read_status(file: &Path, log: &Path) -> Option<Value> {
    let mut status = read_json(file).filter(Value::is_object)?;
    if status["state"] == "running" && status["pid"].as_i64().is_some_and(|p| p != 0 && !is_alive(p)) {
        status["state"] = json!("failed");
        status["error"] = json!(format!("The updater stopped unexpectedly. See {} for details.", log.display()));
    }
    Some(status)
}

pub fn write_status(status: &Value, file: &Path) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", file.display()));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(status).expect("a status serializes")))?;
    std::fs::rename(tmp, file)
}

/// Why this copy can't update itself from the browser, or `None` if it can.
pub fn cannot_apply_reason(install_dir: &Path, home: &Path) -> Option<String> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(crate::node_path::resolve(&p.to_string_lossy())));
    let stable = stable_install_dir(home);
    (real(install_dir) != real(&stable)).then(|| format!("This service runs from {}, not the installed copy in {}.", install_dir.display(), stable.display()))
}

pub struct UpdateInfoOptions {
    pub refresh: bool,
    /// The service can be restarted by just exiting (the launchd job's KeepAlive starts it again).
    pub under_launchd: bool,
    /// The version this process started with, not what's on disk (an update replaces it first).
    pub running_version: Option<String>,
}

/// Everything the extension needs to decide whether to offer an update.
pub async fn get_update_info(checker: &Arc<UpdateChecker>, loc: &Locations, opts: UpdateInfoOptions) -> Value {
    let source = loc.read_install_source();
    let current = opts.running_version.or_else(|| installed_version(&loc.install_dir()));
    // Without `refresh`, never wait on git: answer from the last check and let a stale one refresh
    // in the background (the extension asks every minute).
    let check = if opts.refresh {
        checker.check_for_update(Duration::ZERO, &source).await
    } else {
        let (background, src) = (checker.clone(), source.clone());
        tokio::spawn(async move { background.check_for_update(DEFAULT_CHECK_MAX_AGE, &src).await });
        checker.last_check().unwrap_or_default()
    };
    let reason = cannot_apply_reason(&loc.install_dir(), &loc.home);
    let available = matches!((&check.latest, &current), (Some(l), Some(c)) if compare_versions(l, c) > 0);
    let whats_new: Vec<Value> = current.as_deref().map(|c| whats_new_since(&check.changelog, c, check.latest.as_deref()).iter().map(ChangelogEntry::to_json).collect()).unwrap_or_default();
    json!({
        "current": current, "latest": check.latest, "available": available, "whatsNew": whats_new,
        "changelog": installed_changelog(&loc.install_dir()).iter().map(ChangelogEntry::to_json).collect::<Vec<_>>(),
        "checkedAt": check.checked_at.map(crate::js_number::json_number), "error": check.error,
        "source": source.to_json(), "canApply": reason.is_none(), "cannotApplyReason": reason,
        "restartsItself": opts.under_launchd, "manualCommand": manual_update_command(&source),
        "update": read_status(&status_path(&loc.home), &log_path(&loc.home)),
    })
}

/// How to start the detached runner: the program and its arguments, before the optional
/// `--restart-pid <pid>` (the distribution's binary and its `update-runner` subcommand).
pub struct RunnerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Starts the runner as a detached process (so it outlives this service, which it restarts at the
/// end) and returns the initial status. `service_pid` is only passed when the service runs under
/// launchd, so the runner restarts it by stopping it; otherwise the user restarts it.
pub fn start_update(loc: &Locations, runner: &RunnerCommand, service_pid: Option<u32>) -> Result<Value, String> {
    let home = &loc.home;
    if let Some(existing) = read_status(&status_path(home), &log_path(home)).filter(|s| s["state"] == "running") {
        return Ok(existing);
    }
    if let Some(reason) = cannot_apply_reason(&loc.install_dir(), home) {
        return Err(reason);
    }
    // Written before spawning, so the runner's own (later) writes always win.
    let status = json!({
        "state": "running", "step": "download", "label": "Downloading the latest version\u{2026}",
        "from": installed_version(&loc.install_dir()), "to": null, "pid": null, "startedAt": crate::job_files::now_ms() as i64,
    });
    write_status(&status, &new_status_path(home)).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(new_log_path(home)).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&runner.program);
    cmd.args(&runner.args);
    if let Some(pid) = service_pid {
        cmd.args(["--restart-pid", &pid.to_string()]);
    }
    cmd.current_dir(paths::state_dir(home)).stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut with_pid: Map<String, Value> = status.as_object().cloned().unwrap_or_default();
    with_pid.insert("pid".into(), json!(child.id()));
    Ok(Value::Object(with_pid))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn parse_changelog_reads_release_sections_and_their_bullets_as_plain_text() {
        let text = [
            "# Changelog", "", "- intro bullet, not in a release", "", "## Unreleased", "- not released yet", "",
            "## 0.5.0 \u{2014} 2026-10-01", "- **What's New** in the `update` panel.", "- A long note that", "  wraps onto a second line.", "",
            "Loose text is skipped.", "", "## v0.4.2", "* See [the docs](docs/DESIGN.md).", "", "## 0.4.1",
        ]
        .join("\n");
        assert_eq!(
            parse_changelog(&text),
            vec![
                ChangelogEntry { version: "0.5.0".into(), notes: vec!["What's New in the update panel.".into(), "A long note that wraps onto a second line.".into()] },
                ChangelogEntry { version: "0.4.2".into(), notes: vec!["See the docs.".into()] },
            ]
        );
        assert!(parse_changelog("").is_empty());
    }

    #[test]
    fn whats_new_since_keeps_the_versions_after_the_installed_one_up_to_the_latest_newest_first() {
        let entries: Vec<ChangelogEntry> = ["0.4.0", "0.6.0", "0.4.2", "0.5.0", "0.4.1"].iter().map(|v| ChangelogEntry { version: v.to_string(), notes: vec![v.to_string()] }).collect();
        let versions = |v: Vec<ChangelogEntry>| v.into_iter().map(|e| e.version).collect::<Vec<_>>();
        assert_eq!(versions(whats_new_since(&entries, "0.4.1", Some("0.5.0"))), ["0.5.0", "0.4.2"]);
        assert_eq!(versions(whats_new_since(&entries, "0.4.1", None)), ["0.6.0", "0.5.0", "0.4.2"]);
        assert!(whats_new_since(&entries, "0.6.0", Some("0.6.0")).is_empty());
    }

    #[test]
    fn compare_versions_orders_numerically_not_as_strings() {
        assert_eq!(compare_versions("0.10.0", "0.9.9"), 1);
        assert_eq!(compare_versions("0.4.0", "0.4"), 0);
        assert_eq!(compare_versions("v1.2.3", "1.2.4"), -1);
        assert_eq!(compare_versions("1.2.3-beta", "1.2.3"), 0);
        assert_eq!(compare_versions("", "0.0.1"), -1);
    }

    #[test]
    fn parse_install_source_falls_back_to_the_default_repo_and_rejects_odd_refs() {
        assert_eq!(parse_install_source(None), default_source());
        assert_eq!(parse_install_source(Some(&json!({ "url": " /tmp/repo ", "ref": "release/1.x" }))), Source { url: "/tmp/repo".into(), git_ref: "release/1.x".into() });
        assert_eq!(parse_install_source(Some(&json!({ "url": "/tmp/repo", "ref": "--upload-pack=x y" }))).git_ref, default_source().git_ref);
    }

    #[test]
    fn resolve_install_source_prefers_package_jsons_update_source_then_the_git_origin_then_the_default() {
        let dir = tmp();
        let write_pkg = |pkg: Value| std::fs::write(dir.path().join("package.json"), pkg.to_string()).unwrap();
        let origin = "https://bitbucket.example.com/scm/~me/fork.git";
        let moved = "https://bitbucket.example.com/scm/team/moved.git";

        write_pkg(json!({ "updateSource": { "url": moved } }));
        assert_eq!(resolve_install_source(Some(dir.path()), Some(origin), None, None), Source { url: moved.into(), git_ref: default_source().git_ref });

        write_pkg(json!({ "name": "no-update-source" }));
        assert_eq!(resolve_install_source(Some(dir.path()), Some(origin), None, None), Source { url: origin.into(), git_ref: default_source().git_ref });
        assert_eq!(resolve_install_source(Some(dir.path()), None, None, None), default_source());

        write_pkg(json!({ "updateSource": { "url": moved, "ref": "release" } }));
        assert_eq!(resolve_install_source(Some(dir.path()), None, Some("/tmp/test-repo"), Some("test")), Source { url: "/tmp/test-repo".into(), git_ref: "test".into() });
    }

    #[test]
    fn manual_update_command_gives_the_npx_command_for_a_remote_and_git_pull_for_a_local_checkout() {
        let d = default_source();
        assert_eq!(manual_update_command(&d), format!("npx --yes \"git+{}#{}\"", d.url, d.git_ref));
        assert_eq!(manual_update_command(&Source { url: "git@host:team/repo.git".into(), git_ref: "dev".into() }), "npx --yes \"git+ssh://git@host/team/repo.git#dev\"");
        assert_eq!(manual_update_command(&Source { url: "/Users/me/src/repo".into(), git_ref: "master".into() }), "cd \"/Users/me/src/repo\" && git pull && cargo run --release -- install --yes");
    }

    fn git(dir: &Path, args: &[&str]) {
        assert!(Command::new("git").args(args).current_dir(dir).output().unwrap().status.success(), "git {args:?}");
    }

    #[tokio::test]
    async fn sync_source_clones_the_source_then_picks_up_new_commits() {
        let root = tmp();
        let origin = root.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "master"]);
        git(&origin, &["config", "user.email", "t@example.com"]);
        git(&origin, &["config", "user.name", "t"]);
        std::fs::write(origin.join("package.json"), r#"{"version":"1.0.0"}"#).unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-qm", "v1"]);

        let source = Source { url: origin.to_string_lossy().into_owned(), git_ref: "master".into() };
        let dir = root.path().join("cache");
        sync_source(&source, &dir).await.unwrap();
        assert_eq!(installed_version(&dir).as_deref(), Some("1.0.0"));

        std::fs::write(origin.join("package.json"), r#"{"version":"1.1.0"}"#).unwrap();
        git(&origin, &["commit", "-qam", "v1.1"]);
        sync_source(&source, &dir).await.unwrap();
        assert_eq!(installed_version(&dir).as_deref(), Some("1.1.0"));
    }

    #[tokio::test]
    async fn a_failed_check_is_an_error_in_the_answer_never_a_failure_and_keeps_the_last_good_version() {
        let home = tmp();
        let checker = UpdateChecker::new(home.path().to_path_buf());
        let bad = Source { url: home.path().join("nope").to_string_lossy().into_owned(), git_ref: "master".into() };
        let first = checker.check_for_update(Duration::ZERO, &bad).await;
        assert!(first.error.is_some() && first.latest.is_none());
        *checker.last.lock().unwrap() = Some(Check { latest: Some("1.2.3".into()), ..Default::default() });
        let second = checker.check_for_update(Duration::ZERO, &bad).await;
        assert_eq!(second.latest.as_deref(), Some("1.2.3"));
        assert!(second.error.is_some());
        // Within max age the cached answer is returned without asking again.
        let cached = checker.check_for_update(DEFAULT_CHECK_MAX_AGE, &bad).await;
        assert_eq!(cached.checked_at, second.checked_at);
    }

    #[test]
    fn read_status_reports_a_running_update_whose_runner_is_gone_as_failed() {
        let dir = tmp();
        let file = dir.path().join("status.json");
        let log = dir.path().join("update.log");
        write_status(&json!({ "state": "running", "pid": 999_999, "to": "9.9.9" }), &file).unwrap();
        let failed = read_status(&file, &log).unwrap();
        assert_eq!(failed["state"], "failed");
        assert!(failed["error"].as_str().unwrap().contains("update.log"));
        write_status(&json!({ "state": "running", "pid": std::process::id() }), &file).unwrap();
        assert_eq!(read_status(&file, &log).unwrap()["state"], "running");
        write_status(&json!({ "state": "done", "pid": 999_999 }), &file).unwrap();
        assert_eq!(read_status(&file, &log).unwrap()["state"], "done");
        assert!(read_status(&dir.path().join("none.json"), &log).is_none());
    }

    #[test]
    fn status_and_log_paths_prefer_the_new_state_dirs_file_fall_back_to_the_legacy_one_and_default_to_new() {
        let slug = crate::app_identity::app_slug();
        // Both in the new dir.
        let home = tmp();
        let new_dir = home.path().join(format!(".{slug}"));
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("update-status.json"), "{}").unwrap();
        std::fs::write(new_dir.join("update.log"), "").unwrap();
        assert_eq!(status_path(home.path()), new_dir.join("update-status.json"));
        assert_eq!(log_path(home.path()), new_dir.join("update.log"));

        // Only in the legacy dir.
        let home = tmp();
        let legacy = home.path().join(".bitbucket-ai-companion");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("update-status.json"), "{}").unwrap();
        std::fs::write(legacy.join("update.log"), "").unwrap();
        assert_eq!(status_path(home.path()), legacy.join("update-status.json"));
        assert_eq!(log_path(home.path()), legacy.join("update.log"));
        // Writes never revive the legacy dir.
        assert_eq!(new_status_path(home.path()), home.path().join(format!(".{slug}")).join("update-status.json"));
        assert_eq!(new_log_path(home.path()), home.path().join(format!(".{slug}")).join("update.log"));

        // Neither: the new dir.
        let home = tmp();
        assert_eq!(status_path(home.path()), home.path().join(format!(".{slug}")).join("update-status.json"));
        assert_eq!(log_path(home.path()), home.path().join(format!(".{slug}")).join("update.log"));
    }

    fn loc(home: &Path, install: &Path) -> Locations {
        let service = install.join("companion-service");
        std::fs::create_dir_all(&service).unwrap();
        Locations { companion_service_dir: service, home: home.to_path_buf() }
    }

    #[test]
    fn a_copy_outside_the_stable_install_dir_cannot_apply_and_start_update_refuses() {
        let home = tmp();
        let dev = tmp();
        let l = loc(home.path(), dev.path());
        let reason = cannot_apply_reason(&l.install_dir(), home.path()).unwrap();
        assert!(reason.starts_with("This service runs from "));
        assert!(start_update(&l, &RunnerCommand { program: "/bin/true".into(), args: vec![] }, None).unwrap_err().contains("not the installed copy"));
        // The stable dir itself is fine.
        let stable = stable_install_dir(home.path());
        std::fs::create_dir_all(&stable).unwrap();
        assert_eq!(cannot_apply_reason(&stable, home.path()), None);
    }

    #[test]
    fn start_update_writes_the_status_first_runs_the_runner_detached_and_reuses_a_running_one() {
        let home = tmp();
        let stable = stable_install_dir(home.path());
        let l = loc(home.path(), &stable);
        std::fs::write(stable.join("package.json"), r#"{"version":"0.1.0"}"#).unwrap();
        let args_file = home.path().join("args.txt");
        let script = home.path().join("runner.sh");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" > '{}'\necho ran\n", args_file.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // Run through `sh <script>` rather than executing the file just written: under load another
        // thread's fork can hold its write handle open ("text file busy").
        let runner = RunnerCommand { program: "/bin/sh".into(), args: vec![script.to_string_lossy().into_owned(), "update-runner".into()] };

        let status = start_update(&l, &runner, Some(4242)).unwrap();
        assert_eq!((status["state"].clone(), status["from"].clone()), (json!("running"), json!("0.1.0")));
        assert!(status["pid"].as_u64().unwrap() > 0);
        for _ in 0..200 {
            if args_file.exists() && std::fs::read_to_string(&args_file).is_ok_and(|t| t.contains("4242")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&args_file).unwrap().trim(), "update-runner --restart-pid 4242");
        assert_eq!(std::fs::read_to_string(new_log_path(home.path())).unwrap().trim(), "ran");

        // A status whose runner is alive is returned as it is, nothing new starts.
        write_status(&json!({ "state": "running", "pid": std::process::id(), "marker": 1 }), &new_status_path(home.path())).unwrap();
        assert_eq!(start_update(&l, &runner, None).unwrap()["marker"], 1);
    }

    #[tokio::test]
    async fn get_update_info_carries_this_copys_own_changelog_newest_first_for_settings() {
        let home = tmp();
        let dev = tmp();
        let l = loc(home.path(), dev.path());
        std::fs::write(dev.path().join("package.json"), r#"{"version":"0.10.0"}"#).unwrap();
        std::fs::write(dev.path().join("CHANGELOG.md"), "# Changelog\n\n## Unreleased\n\n- not out yet\n\n## 0.9.0\n\n- Older **change**\n\n## 0.10.0\n\n- Newer change\n").unwrap();
        let checker = UpdateChecker::new(home.path().to_path_buf());
        let info = get_update_info(&checker, &l, UpdateInfoOptions { refresh: false, under_launchd: false, running_version: None }).await;
        let versions: Vec<&str> = info["changelog"].as_array().unwrap().iter().map(|e| e["version"].as_str().unwrap()).collect();
        assert_eq!(versions, ["0.10.0", "0.9.0"]);
        assert_eq!(info["changelog"][1]["notes"][0], "Older change");
    }

    #[test]
    fn find_companion_service_dir_walks_up_to_the_package_and_skips_dist() {
        let root = tmp();
        let service = root.path().join("companion-service");
        let deep = service.join("dist").join("core");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(service.join("package.json"), r#"{"name":"bitbucket-ai-companion"}"#).unwrap();
        std::fs::write(service.join("dist").join("package.json"), r#"{"name":"bitbucket-ai-companion"}"#).unwrap();
        assert_eq!(find_companion_service_dir(&deep), service);
    }

    #[tokio::test]
    async fn get_update_info_answers_from_the_last_check_without_waiting_and_names_the_manual_command() {
        let home = tmp();
        let dev = tmp();
        let l = loc(home.path(), dev.path());
        std::fs::write(dev.path().join("package.json"), r#"{"version":"0.1.0"}"#).unwrap();
        std::fs::write(l.install_source_path(), json!({ "url": "/nowhere/repo", "ref": "main" }).to_string()).unwrap();
        let checker = UpdateChecker::new(home.path().to_path_buf());
        *checker.last.lock().unwrap() = Some(Check { latest: Some("0.2.0".into()), changelog: vec![ChangelogEntry { version: "0.2.0".into(), notes: vec!["New".into()] }], error: None, checked_at: Some(crate::job_files::now_ms()) });
        let info = get_update_info(&checker, &l, UpdateInfoOptions { refresh: false, under_launchd: true, running_version: None }).await;
        assert_eq!((info["current"].clone(), info["latest"].clone(), info["available"].clone()), (json!("0.1.0"), json!("0.2.0"), json!(true)));
        assert_eq!(info["whatsNew"][0]["version"], "0.2.0");
        // No CHANGELOG.md in this copy: an empty list, not an error.
        assert_eq!(info["changelog"], json!([]));
        assert_eq!((info["canApply"].clone(), info["restartsItself"].clone()), (json!(false), json!(true)));
        assert_eq!(info["manualCommand"], "cd \"/nowhere/repo\" && git pull && cargo run --release -- install --yes");
        assert_eq!(info["source"], json!({ "url": "/nowhere/repo", "ref": "main" }));
    }
}
