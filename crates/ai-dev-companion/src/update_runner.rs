//! Applies an update (port of `core/update-runner.js`), started detached by `updater::start_update`
//! so it outlives the service it restarts. Progress goes to `update-status.json`, which the (old,
//! then new) service reports to the extension; output goes to `update.log`.
//!
//! Steps: sync the install source, run its installer non-interactively without the service
//! (copies the new files over this install, keeps `config.json`, rebuilds), then, if the service
//! runs under launchd, stop it so the job's KeepAlive starts the new build. launchd itself is
//! never unloaded or reloaded here, so an interrupted update can't leave the background job missing.

use crate::updater::{self, Locations, Source};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The installer's program and arguments, given the staging dir.
pub type InstallerFn = Box<dyn Fn(&Path) -> (PathBuf, Vec<String>) + Send + Sync>;

/// What the runner needs besides the install source on disk.
pub struct RunnerEnv {
    pub loc: Locations,
    /// Runs the staged copy's installer: given the staging dir, the program and its arguments (non-
    /// interactive, no service), e.g. `cargo run --release -- install --yes --no-service`.
    pub installer: InstallerFn,
    /// Stops the service so launchd's KeepAlive starts the new build.
    pub stop_service: Box<dyn Fn(u32) + Send + Sync>,
    pub log: Box<dyn Fn(&str) + Send + Sync>,
}

/// `--restart-pid <pid>` from the runner's arguments.
pub fn parse_restart_pid<S: AsRef<str>>(argv: &[S]) -> Option<u32> {
    let i = argv.iter().position(|a| a.as_ref() == "--restart-pid")?;
    argv.get(i + 1)?.as_ref().parse::<u32>().ok().filter(|p| *p != 0)
}

struct Reporter<'a> {
    env: &'a RunnerEnv,
    from: Option<String>,
    started_at: i64,
    to: Option<String>,
}

impl Reporter<'_> {
    fn report(&self, fields: Value) {
        let mut status = Map::new();
        status.insert("from".into(), json!(self.from));
        status.insert("pid".into(), json!(std::process::id()));
        status.insert("startedAt".into(), json!(self.started_at));
        status.insert("to".into(), json!(self.to));
        if let Value::Object(f) = &fields {
            status.extend(f.clone());
        }
        let _ = updater::write_status(&Value::Object(status), &updater::new_status_path(&self.env.loc.home));
        let text = |k: &str| fields[k].as_str().filter(|s| !s.is_empty()).map(|s| format!(": {s}")).unwrap_or_default();
        (self.env.log)(&format!("[update] {}{}{}", fields["state"].as_str().unwrap_or(""), text("label"), text("error")));
    }
}

async fn apply(env: &RunnerEnv, reporter: &mut Reporter<'_>, restart_pid: Option<u32>) -> Result<(), String> {
    reporter.report(json!({ "state": "running", "step": "download", "label": "Downloading the latest version\u{2026}" }));
    let source: Source = env.loc.read_install_source();
    let dir = updater::sync_source(&source, &updater::staging_dir(&env.loc.home)).await?;
    let to = updater::installed_version(&dir);
    reporter.to = to.clone();
    let shown = to.unwrap_or_default();

    reporter.report(json!({ "state": "running", "step": "install", "label": format!("Installing v{shown} (this takes a minute)\u{2026}") }));
    let (program, args) = (env.installer)(&dir);
    let status = tokio::process::Command::new(&program).args(&args).current_dir(&dir).status().await.map_err(|e| format!("The installer couldn't start: {e}"))?;
    if !status.success() {
        let code = status.code().map_or("?".to_string(), |c| c.to_string());
        return Err(format!("The installer exited with code {code}. See {}.", updater::log_path(&env.loc.home).display()));
    }

    let finished = crate::job_files::now_ms() as i64;
    let Some(pid) = restart_pid else {
        reporter.report(json!({ "state": "done", "needsRestart": true, "finishedAt": finished, "label": format!("Installed v{shown}. Restart the companion service to finish.") }));
        return Ok(());
    };
    reporter.report(json!({ "state": "done", "needsRestart": false, "finishedAt": finished, "label": format!("Installed v{shown}. Restarting the companion service\u{2026}") }));
    // Already gone is fine: launchd starts the new build either way.
    (env.stop_service)(pid);
    Ok(())
}

/// Runs the whole update; the process exit code (0 done, 1 failed).
pub async fn run(env: &RunnerEnv, restart_pid: Option<u32>) -> i32 {
    let mut reporter = Reporter { env, from: updater::installed_version(&env.loc.install_dir()), started_at: crate::job_files::now_ms() as i64, to: None };
    match apply(env, &mut reporter, restart_pid).await {
        Ok(()) => 0,
        Err(message) => {
            let source = env.loc.read_install_source();
            reporter.report(json!({
                "state": "failed", "finishedAt": crate::job_files::now_ms() as i64,
                "error": format!("{message}\n\nNothing was changed if this happened while downloading. To update by hand, run: {}", updater::manual_update_command(&source)),
            }));
            1
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_restart_pid_is_read_from_the_arguments() {
        assert_eq!(parse_restart_pid(&["--restart-pid", "123"]), Some(123));
        assert_eq!(parse_restart_pid(&["x", "--restart-pid", "9", "y"]), Some(9));
        for bad in [vec![], vec!["--restart-pid"], vec!["--restart-pid", "abc"], vec!["--restart-pid", "0"]] {
            assert_eq!(parse_restart_pid(&bad), None);
        }
    }

    struct Fx {
        _d: tempfile::TempDir,
        home: PathBuf,
        env: RunnerEnv,
        stopped: Arc<Mutex<Vec<u32>>>,
        logs: Arc<Mutex<Vec<String>>>,
    }

    fn git(dir: &Path, args: &[&str]) {
        assert!(Command::new("git").args(args).current_dir(dir).output().unwrap().status.success());
    }

    /// An install whose source is a local git repo at version 2.0.0, and an installer that is `sh -c`.
    fn fx(installer_script: &'static str) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let home = root.join("home");
        let install = root.join("install");
        let service = install.join("companion-service");
        std::fs::create_dir_all(&service).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(install.join("package.json"), r#"{"version":"1.0.0"}"#).unwrap();
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "release"]);
        git(&origin, &["config", "user.email", "t@example.com"]);
        git(&origin, &["config", "user.name", "t"]);
        std::fs::write(origin.join("package.json"), r#"{"version":"2.0.0"}"#).unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-qm", "v2"]);
        std::fs::write(service.join("install-source.json"), json!({ "url": origin, "ref": "release" }).to_string()).unwrap();
        let (stopped, logs) = (Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![])));
        let (s, l) = (stopped.clone(), logs.clone());
        let env = RunnerEnv {
            loc: Locations { companion_service_dir: service, home: home.clone() },
            installer: Box::new(move |_| ("sh".into(), vec!["-c".into(), installer_script.into()])),
            stop_service: Box::new(move |pid| s.lock().unwrap().push(pid)),
            log: Box::new(move |line| l.lock().unwrap().push(line.to_string())),
        };
        Fx { _d: d, home, env, stopped, logs }
    }

    fn status(f: &Fx) -> Value {
        updater::read_json(&updater::new_status_path(&f.home)).unwrap()
    }

    #[tokio::test]
    async fn without_a_restart_pid_the_update_is_installed_and_asks_for_a_manual_restart() {
        let f = fx("exit 0");
        assert_eq!(run(&f.env, None).await, 0);
        let s = status(&f);
        assert_eq!((s["state"].clone(), s["needsRestart"].clone(), s["from"].clone(), s["to"].clone()), (json!("done"), json!(true), json!("1.0.0"), json!("2.0.0")));
        assert!(s["label"].as_str().unwrap().contains("Restart the companion service"));
        assert!(f.stopped.lock().unwrap().is_empty());
        assert_eq!(f.logs.lock().unwrap()[0], "[update] running: Downloading the latest version\u{2026}");
    }

    #[tokio::test]
    async fn with_a_restart_pid_the_service_is_stopped_after_the_install() {
        let f = fx("exit 0");
        assert_eq!(run(&f.env, Some(4242)).await, 0);
        let s = status(&f);
        assert_eq!((s["state"].clone(), s["needsRestart"].clone()), (json!("done"), json!(false)));
        assert_eq!(*f.stopped.lock().unwrap(), [4242]);
    }

    #[tokio::test]
    async fn a_failing_installer_fails_the_update_with_the_manual_command_and_does_not_stop_the_service() {
        let f = fx("exit 3");
        assert_eq!(run(&f.env, Some(4242)).await, 1);
        let s = status(&f);
        assert_eq!(s["state"], "failed");
        let error = s["error"].as_str().unwrap();
        assert!(error.contains("The installer exited with code 3.") && error.contains("To update by hand, run: "), "{error}");
        assert!(f.stopped.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_source_fails_while_downloading() {
        let f = fx("exit 0");
        std::fs::write(f.env.loc.install_source_path(), json!({ "url": f.home.join("nowhere"), "ref": "release" }).to_string()).unwrap();
        assert_eq!(run(&f.env, None).await, 1);
        assert_eq!(status(&f)["state"], "failed");
        assert!(f.logs.lock().unwrap().last().unwrap().starts_with("[update] failed"));
    }
}
