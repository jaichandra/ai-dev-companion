//! macOS background-service management via launchd (port of
//! `core/launchd.js`): a per-user LaunchAgent, with no sudo or root needed
//! (unlike a system-level LaunchDaemon).
//!
//! Split deliberately into pure, directly-testable helpers (`render_plist`,
//! `build_service_path`, `parse_status`) and thin side-effecting wrappers
//! (`install_service`, `uninstall_service`, `service_status`) that shell out to
//! `launchctl`: the latter are real, externally visible actions (installing a
//! real launchd job on whatever machine runs the test), not logic to verify.
//!
//! The job runs this very binary (`<binary> serve`), so there is no Node path
//! and no system-CA environment variable to bake in: the HTTP client already
//! trusts the system certificate store.

use crate::app_identity::app_slug;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The job's label. Overridable so a second copy (a dev checkout) can run as
/// its own job without replacing the installed one.
pub fn label() -> String {
    std::env::var("AI_DEV_COMPANION_SERVICE_LABEL").ok().filter(|l| !l.is_empty()).unwrap_or_else(|| format!("com.{}.companion-service", app_slug()))
}

pub fn plist_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library").join("LaunchAgents").join(format!("{label}.plist"))
}

/// Where the job's stdout and stderr go: two distinct files in `dir`.
pub fn log_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("companion-service.out.log"), dir.join("companion-service.err.log"))
}

fn escape_xml(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// What the plist runs and where.
pub struct PlistSpec<'a> {
    pub label: &'a str,
    /// The program and its arguments: the absolute path of this binary, then `serve`.
    pub program_arguments: &'a [String],
    pub working_dir: &'a str,
    pub path_env: &'a str,
    pub out_log: &'a str,
    pub err_log: &'a str,
}

/// Pure plist rendering: every path and value that matters is passed in
/// explicitly, so this is testable without touching the filesystem or the
/// environment. `program_arguments[0]` is deliberately an *absolute* path and
/// `path_env` deliberately the *caller's* PATH: launchd runs LaunchAgents with
/// a minimal environment (no `~/.zshrc`, no homebrew shims), so without this,
/// `git`/`claude`, which are invoked by bare name, would resolve against
/// launchd's bare-bones PATH instead of the one this was configured under, and
/// silently fail as if git/claude were missing.
pub fn render_plist(spec: &PlistSpec<'_>) -> String {
    let args: String = spec.program_arguments.iter().map(|a| format!("    <string>{}</string>\n", escape_xml(a))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>WorkingDirectory</key>
  <string>{working_dir}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path_env}</string>
    <key>AI_DEV_COMPANION_UNDER_LAUNCHD</key>
    <string>1</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>StandardOutPath</key>
  <string>{out_log}</string>
  <key>StandardErrorPath</key>
  <string>{err_log}</string>
</dict>
</plist>
"#,
        label = escape_xml(spec.label),
        working_dir = escape_xml(spec.working_dir),
        path_env = escape_xml(spec.path_env),
        out_log = escape_xml(spec.out_log),
        err_log = escape_xml(spec.err_log),
    )
}

const STANDARD_PATH_DIRS: [&str; 6] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// The PATH to bake into the plist: the caller's PATH minus entries that only
/// exist for the duration of an `npm run`/`npx` invocation, plus the standard
/// system dirs if missing, so git/claude still resolve even from a stripped-down shell.
pub fn build_service_path(path_env: Option<&str>) -> String {
    let transient = |dir: &str| dir.contains("node_modules/.bin") || dir.contains("node_modules\\.bin") || dir.contains("node-gyp-bin") || dir.contains("/_npx/") || dir.contains("\\_npx\\");
    let mut out: Vec<&str> = vec![];
    for dir in path_env.unwrap_or("").split(':').filter(|d| !d.is_empty() && !transient(d)).chain(STANDARD_PATH_DIRS) {
        if !out.contains(&dir) {
            out.push(dir);
        }
    }
    out.join(":")
}

fn gui_domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("launchctl").args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())
}

fn is_loaded(label: &str) -> bool {
    launchctl(&["print", &format!("{}/{label}", gui_domain())]).is_ok_and(|o| o.status.success())
}

/// Unloads whatever's currently loaded under this label and waits for launchd to
/// actually finish: `bootout` returns before the job is gone, and bootstrapping
/// again too soon fails with "5: Input/output error".
fn unload_if_loaded(label: &str) {
    if !is_loaded(label) {
        return;
    }
    let _ = launchctl(&["bootout", &format!("{}/{label}", gui_domain())]);
    for _ in 0..50 {
        if !is_loaded(label) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn bootstrap_with_retry(target: &Path) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..5u64 {
        match launchctl(&["bootstrap", &gui_domain(), &target.to_string_lossy()]) {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => last = String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(500 * (attempt + 1)));
    }
    Err(format!("launchctl could not load {}: {last}", target.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub label: String,
    pub plist_path: PathBuf,
    pub out_log: PathBuf,
    pub err_log: PathBuf,
}

/// Writes the plist and (re)loads it under launchd. The binary must exist: a
/// background job pointed at a file that doesn't exist would just crash-loop
/// silently, so that's checked up front with a clear error instead.
pub fn install_service(home: &Path, binary: &Path, logs_dir: &Path, working_dir: &Path, path_env: Option<&str>) -> Result<Installed, String> {
    if !binary.exists() {
        return Err(format!("{} doesn't exist yet \u{2014} build it first, then re-run this.", binary.display()));
    }
    let label = label();
    let (out, err) = log_paths(logs_dir);
    fs::create_dir_all(logs_dir).map_err(|e| e.to_string())?;
    let args = [binary.to_string_lossy().into_owned(), "serve".to_string()];
    let plist = render_plist(&PlistSpec {
        label: &label,
        program_arguments: &args,
        working_dir: &working_dir.to_string_lossy(),
        path_env: &build_service_path(path_env),
        out_log: &out.to_string_lossy(),
        err_log: &err.to_string_lossy(),
    });
    let target = plist_path(home, &label);
    fs::create_dir_all(target.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
    unload_if_loaded(&label);
    fs::write(&target, plist).map_err(|e| e.to_string())?;
    bootstrap_with_retry(&target)?;
    Ok(Installed { label, plist_path: target, out_log: out, err_log: err })
}

pub fn is_installed(home: &Path) -> bool {
    plist_path(home, &label()).exists()
}

/// Unloads the job and deletes its plist. Returns false if there was nothing
/// installed to remove (idempotent: safe to run more than once, or on a machine
/// that never had it installed).
pub fn uninstall_service(home: &Path) -> bool {
    let label = label();
    let target = plist_path(home, &label);
    let existed = target.exists();
    unload_if_loaded(&label);
    if existed {
        let _ = fs::remove_file(&target);
    }
    existed
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub loaded: bool,
    /// `None` if the job is loaded but not currently running (it crashed and
    /// launchd hasn't respawned it yet).
    pub pid: Option<String>,
}

/// `launchctl list <label>`'s output. Modern launchctl prints a plist-like
/// dictionary, e.g. `"PID" = 8478;` when running and no PID key at all when
/// loaded but not running. The older documented output is a single
/// `PID\tStatus\tLabel` line (`-` for PID when not running), kept as a fallback.
pub fn parse_status(output: &str) -> Status {
    static PID: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#""PID"\s*=\s*(\d+)\s*;"#).unwrap());
    if let Some(m) = PID.captures(output) {
        return Status { loaded: true, pid: Some(m[1].to_string()) };
    }
    let trimmed = output.trim();
    if trimmed.starts_with('{') {
        return Status { loaded: true, pid: None };
    }
    let first = trimmed.split_whitespace().next();
    Status { loaded: true, pid: first.filter(|f| *f != "-").map(String::from) }
}

pub fn service_status() -> Status {
    match launchctl(&["list", &label()]) {
        Ok(o) if o.status.success() => parse_status(&String::from_utf8_lossy(&o.stdout)),
        _ => Status { loaded: false, pid: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(args: &'a [String], working_dir: &'a str, path_env: &'a str, out: &'a str, err: &'a str) -> PlistSpec<'a> {
        PlistSpec { label: "com.example.test", program_arguments: args, working_dir, path_env, out_log: out, err_log: err }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_plist_path_lives_under_library_launch_agents_and_is_named_after_the_label() {
        assert_eq!(plist_path(Path::new("/Users/x"), "com.example.test"), Path::new("/Users/x/Library/LaunchAgents/com.example.test.plist"));
        assert!(label().starts_with("com.") && label().ends_with(".companion-service") || std::env::var("AI_DEV_COMPANION_SERVICE_LABEL").is_ok());
    }

    #[test]
    fn log_paths_are_two_distinct_files_in_the_given_dir() {
        let (out, err) = log_paths(Path::new("/s"));
        assert_ne!(out, err);
        assert!(out.ends_with("companion-service.out.log") && err.ends_with("companion-service.err.log"));
    }

    #[test]
    fn render_plist_runs_the_binary_with_serve_and_keeps_the_job_alive() {
        let a = args(&["/Users/x/bin/companion", "serve"]);
        let xml = render_plist(&spec(&a, "/Users/x/state", "/usr/local/bin:/usr/bin:/bin", "/Users/x/o.log", "/Users/x/e.log"));
        for want in [
            "<string>com.example.test</string>",
            "<array>\n    <string>/Users/x/bin/companion</string>\n    <string>serve</string>\n  </array>",
            "<string>/Users/x/state</string>",
            "<string>/usr/local/bin:/usr/bin:/bin</string>",
            "<key>AI_DEV_COMPANION_UNDER_LAUNCHD</key>\n    <string>1</string>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>",
            "<key>ThrottleInterval</key>\n  <integer>5</integer>",
            "<string>/Users/x/o.log</string>",
            "<string>/Users/x/e.log</string>",
        ] {
            assert!(xml.contains(want), "missing {want:?} in\n{xml}");
        }
        assert!(!xml.contains("NODE_USE_SYSTEM_CA"), "there is no Node to configure");
    }

    #[test]
    fn render_plist_escapes_xml_special_characters_in_every_value() {
        let a = args(&["/Users/x & y/companion", "serve", "<a>"]);
        let xml = render_plist(&spec(&a, "/Users/x & y", "/usr/bin", "/tmp/o'.log", "/tmp/\"e\".log"));
        assert!(xml.contains("/Users/x &amp; y/companion") && xml.contains("&lt;a&gt;") && xml.contains("o&apos;.log") && xml.contains("&quot;e&quot;"));
        assert!(!xml.contains("x & y"));
    }

    #[test]
    fn render_plist_produces_balanced_tags() {
        let a = args(&["/x/companion", "serve"]);
        let xml = render_plist(&spec(&a, "/x", "/usr/bin", "/tmp/o", "/tmp/e"));
        let count = |needle: &str| xml.matches(needle).count();
        for tag in ["dict", "array", "plist"] {
            assert_eq!(count(&format!("<{tag}>")) + count(&format!("<{tag} ")), count(&format!("</{tag}>")), "{tag}");
        }
    }

    #[test]
    fn build_service_path_drops_npm_and_npx_only_entries_and_appends_missing_system_dirs() {
        let result = build_service_path(Some(
            "/Users/x/ai-dev-companion/node_modules/.bin:/Users/x/.nvm/versions/node/v24/lib/node_modules/npm/node_modules/@npmcli/run-script/lib/node-gyp-bin:/Users/x/.npm/_npx/abc123/node_modules/.bin:/Users/x/.nvm/versions/node/v24/bin:/usr/bin",
        ));
        let dirs: Vec<&str> = result.split(':').collect();
        assert_eq!(dirs[0], "/Users/x/.nvm/versions/node/v24/bin");
        assert!(!dirs.iter().any(|d| d.contains("node_modules") || d.contains("_npx")));
        assert_eq!(dirs.iter().filter(|d| **d == "/usr/bin").count(), 1);
        for dir in ["/usr/local/bin", "/opt/homebrew/bin", "/bin"] {
            assert!(dirs.contains(&dir), "{dir}");
        }
    }

    #[test]
    fn build_service_path_tolerates_an_empty_path() {
        assert_eq!(build_service_path(None), "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(build_service_path(Some("::")), build_service_path(None));
    }

    #[test]
    fn parse_status_reads_the_modern_dictionary_and_the_old_table() {
        assert_eq!(parse_status("{\n\t\"Label\" = \"com.x\";\n\t\"PID\" = 8478;\n}"), Status { loaded: true, pid: Some("8478".into()) });
        assert_eq!(parse_status("{\n\t\"Label\" = \"com.x\";\n\t\"LastExitStatus\" = 256;\n}"), Status { loaded: true, pid: None });
        assert_eq!(parse_status("123\t0\tcom.x\n"), Status { loaded: true, pid: Some("123".into()) });
        assert_eq!(parse_status("-\t0\tcom.x\n"), Status { loaded: true, pid: None });
    }

    #[test]
    fn install_refuses_a_binary_that_does_not_exist_before_touching_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let e = install_service(dir.path(), &dir.path().join("missing"), dir.path(), dir.path(), None).unwrap_err();
        assert!(e.contains("doesn't exist yet"), "{e}");
        assert!(!dir.path().join("Library").exists());
    }
}
