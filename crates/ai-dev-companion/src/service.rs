//! `companion service <install|uninstall|status>` (port of `service.js`): manages the companion as
//! a background job via launchd: a per-user LaunchAgent that starts it at login and restarts it
//! if it crashes, so no terminal has to stay open. macOS only (launchd is); elsewhere it says so
//! and fails.

use crate::launchd::{self, Installed, Status};
use std::path::{Path, PathBuf};

/// What the command does to the machine, injectable so tests never touch launchd.
pub trait ServiceOps {
    fn is_macos(&self) -> bool;
    fn install(&self) -> Result<Installed, String>;
    /// False when nothing was installed.
    fn uninstall(&self) -> bool;
    fn status(&self) -> Status;
    fn plist_path(&self) -> PathBuf;
    fn err_log(&self) -> PathBuf;
}

/// The real launchd job for the binary at `binary`, running in `working_dir`, logging to `logs_dir`.
pub struct LaunchdService {
    pub home: PathBuf,
    pub binary: PathBuf,
    pub logs_dir: PathBuf,
    pub working_dir: PathBuf,
    pub path_env: Option<String>,
}

impl ServiceOps for LaunchdService {
    fn is_macos(&self) -> bool {
        cfg!(target_os = "macos")
    }
    fn install(&self) -> Result<Installed, String> {
        launchd::install_service(&self.home, &self.binary, &self.logs_dir, &self.working_dir, self.path_env.as_deref())
    }
    fn uninstall(&self) -> bool {
        launchd::uninstall_service(&self.home)
    }
    fn status(&self) -> Status {
        launchd::service_status()
    }
    fn plist_path(&self) -> PathBuf {
        launchd::plist_path(&self.home, &launchd::label())
    }
    fn err_log(&self) -> PathBuf {
        launchd::log_paths(&self.logs_dir).1
    }
}

fn show(p: &Path) -> String {
    p.display().to_string()
}

/// Runs the subcommand; `out` and `err` get whole lines (no newline). The exit code.
pub fn run_service(cmd: Option<&str>, ops: &dyn ServiceOps, out: &mut dyn FnMut(&str), err: &mut dyn FnMut(&str)) -> i32 {
    if !ops.is_macos() {
        err("Background-service mode uses launchd, which is macOS-only. On other platforms, run `companion serve` in a terminal (or set this up under your own systemd/pm2/etc. instead).");
        return 1;
    }
    match cmd {
        Some("install") => match ops.install() {
            Ok(info) => {
                out("Installed and started the companion service as a background job.");
                out(&format!("  Label: {}", info.label));
                out(&format!("  Plist: {}", show(&info.plist_path)));
                out(&format!("  Logs:  {}", show(&info.out_log)));
                out(&format!("         {}", show(&info.err_log)));
                out("\nIt will now start automatically at login and restart itself if it crashes \u{2014} no terminal needs to stay open. Re-run `companion service install` any time to restart it. `companion doctor` checks that everything is connected.");
                0
            }
            Err(e) => {
                err(&format!("Failed: {e}"));
                1
            }
        },
        Some("uninstall") => {
            if ops.uninstall() {
                out(&format!("Stopped and removed the background service ({}).", show(&ops.plist_path())));
            } else {
                out("No background service was installed \u{2014} nothing to do.");
            }
            0
        }
        Some("status") => {
            let status = ops.status();
            if !status.loaded {
                out("Not installed as a background service. Run `companion service install` to set it up.");
            } else if let Some(pid) = status.pid {
                out(&format!("Running as a background service (pid {pid})."));
            } else {
                out(&format!("Installed, but not currently running (it may have crashed and not been respawned yet) \u{2014} check {}", show(&ops.err_log())));
            }
            out(&format!("Plist: {}", show(&ops.plist_path())));
            0
        }
        _ => {
            err("Usage: companion service <install|uninstall|status>");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        macos: bool,
        install: Result<Installed, String>,
        existed: bool,
        status: Status,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                macos: true,
                install: Ok(Installed { label: "com.x.svc".into(), plist_path: "/h/Library/LaunchAgents/com.x.svc.plist".into(), out_log: "/l/out.log".into(), err_log: "/l/err.log".into() }),
                existed: true,
                status: Status { loaded: true, pid: Some("42".into()) },
                calls: RefCell::new(vec![]),
            }
        }
    }

    impl ServiceOps for Fake {
        fn is_macos(&self) -> bool {
            self.macos
        }
        fn install(&self) -> Result<Installed, String> {
            self.calls.borrow_mut().push("install");
            self.install.clone()
        }
        fn uninstall(&self) -> bool {
            self.calls.borrow_mut().push("uninstall");
            self.existed
        }
        fn status(&self) -> Status {
            self.status.clone()
        }
        fn plist_path(&self) -> PathBuf {
            "/h/Library/LaunchAgents/com.x.svc.plist".into()
        }
        fn err_log(&self) -> PathBuf {
            "/l/err.log".into()
        }
    }

    fn run(cmd: Option<&str>, ops: &Fake) -> (i32, String, String) {
        let (mut out, mut err) = (String::new(), String::new());
        let code = run_service(cmd, ops, &mut |l| out.push_str(&format!("{l}\n")), &mut |l| err.push_str(&format!("{l}\n")));
        (code, out, err)
    }

    #[test]
    fn install_reports_the_job_its_files_and_logs() {
        let (code, out, err) = run(Some("install"), &Fake::new());
        assert_eq!(code, 0, "{err}");
        for needle in ["Label: com.x.svc", "Plist: /h/Library/LaunchAgents/com.x.svc.plist", "Logs:  /l/out.log", "/l/err.log", "start automatically at login"] {
            assert!(out.contains(needle), "{needle} in {out}");
        }
    }

    #[test]
    fn a_failed_install_is_a_message_and_exit_1() {
        let mut f = Fake::new();
        f.install = Err("/bin/x doesn't exist yet".into());
        let (code, out, err) = run(Some("install"), &f);
        assert_eq!((code, out.as_str(), err.as_str()), (1, "", "Failed: /bin/x doesn't exist yet\n"));
    }

    #[test]
    fn uninstall_says_whether_there_was_anything_to_remove_and_is_never_a_failure() {
        let (code, out, _) = run(Some("uninstall"), &Fake::new());
        assert!(code == 0 && out.contains("Stopped and removed the background service (/h/Library/LaunchAgents/com.x.svc.plist)."), "{out}");
        let mut none = Fake::new();
        none.existed = false;
        let (code, out, _) = run(Some("uninstall"), &none);
        assert!(code == 0 && out.contains("nothing to do"), "{out}");
    }

    #[test]
    fn status_tells_running_crashed_and_not_installed_apart_and_always_names_the_plist() {
        let (_, out, _) = run(Some("status"), &Fake::new());
        assert!(out.contains("Running as a background service (pid 42).") && out.contains("Plist: /h/"), "{out}");
        let mut crashed = Fake::new();
        crashed.status = Status { loaded: true, pid: None };
        let (_, out, _) = run(Some("status"), &crashed);
        assert!(out.contains("not currently running") && out.contains("check /l/err.log"), "{out}");
        let mut off = Fake::new();
        off.status = Status { loaded: false, pid: None };
        let (code, out, _) = run(Some("status"), &off);
        assert!(code == 0 && out.contains("Not installed as a background service"), "{out}");
    }

    #[test]
    fn other_platforms_and_unknown_commands_fail_with_a_message_and_touch_nothing() {
        let mut linux = Fake::new();
        linux.macos = false;
        let (code, _, err) = run(Some("install"), &linux);
        assert!(code == 1 && err.contains("macOS-only") && linux.calls.borrow().is_empty(), "{err}");
        for cmd in [None, Some("restart")] {
            let f = Fake::new();
            let (code, _, err) = run(cmd, &f);
            assert!(code == 1 && err.starts_with("Usage: companion service"), "{err}");
            assert!(f.calls.borrow().is_empty());
        }
    }
}
