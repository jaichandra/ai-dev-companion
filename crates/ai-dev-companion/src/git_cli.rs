//! Running `git` for the features that work in clones and worktrees (the
//! `defaultGit` of `core/ticket-git.js`): an argv array (no shell), no stdin, a
//! timeout that kills the whole process group, and error text with URL
//! credentials and tokens masked (it ends up in `job.error`). The [`Git`]
//! trait lets the pure-ish planning code run against a fake in tests.

use crate::history_record::redact_secrets;
use crate::js_text::{js_trim, slice_utf16};
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// What a fetch of a big repo is given.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOut {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct GitOpts {
    pub allow_failure: bool,
    pub timeout: Duration,
}

impl Default for GitOpts {
    fn default() -> Self {
        Self { allow_failure: false, timeout: GIT_TIMEOUT }
    }
}

impl GitOpts {
    /// A non-zero exit is an answer, not an error.
    pub fn allow_failure() -> Self {
        Self { allow_failure: true, ..Self::default() }
    }

    pub fn timeout(timeout: Duration) -> Self {
        Self { timeout, ..Self::default() }
    }

    pub fn with_allow_failure(mut self, allow: bool) -> Self {
        self.allow_failure = allow;
        self
    }
}

pub trait Git: Send + Sync {
    fn run(&self, args: &[&str], cwd: &Path, opts: GitOpts) -> impl Future<Output = Result<GitOut, String>> + Send;
}

/// The real `git`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGit;

impl Git for SystemGit {
    async fn run(&self, args: &[&str], cwd: &Path, opts: GitOpts) -> Result<GitOut, String> {
        let first = args.first().copied().unwrap_or("");
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("GIT_TERMINAL_PROMPT", "0");
        // Its own process group, so a timeout can stop a hook or helper it started too.
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn().map_err(|e| redact_secrets(&e.to_string()))?;
        let pid = child.id();
        let out = match tokio::time::timeout(opts.timeout, child.wait_with_output()).await {
            Ok(out) => out.map_err(|e| redact_secrets(&e.to_string()))?,
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = pid {
                    // SAFETY: signalling the process group this call created.
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                return Err(format!("git {first} timed out after {} s and was stopped.", opts.timeout.as_secs_f64().round() as u64));
            }
        };
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        let code = out.status.code().unwrap_or(-1);
        if code != 0 && !opts.allow_failure {
            let detail = if stderr.is_empty() { &stdout } else { &stderr };
            return Err(redact_secrets(&format!("git {first} failed (exit {code}): {}", slice_utf16(js_trim(detail), 2000))));
        }
        Ok(GitOut { stdout, stderr, code })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Throwaway real repos, and a canned `Git`.
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Runs git synchronously in a test fixture and returns its trimmed stdout.
    pub fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").args(args).current_dir(cwd).output().expect("git runs");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn git_fails(cwd: &Path, args: &[&str]) -> bool {
        !std::process::Command::new("git").args(args).current_dir(cwd).output().expect("git runs").status.success()
    }

    /// A bare "origin" with one commit on master, and a clone of it, all in a
    /// temp dir, no network. Paths are real (macOS /var is a link to /private/var).
    pub struct Repos {
        _dir: tempfile::TempDir,
        pub root: PathBuf,
        pub origin: PathBuf,
        pub clone: PathBuf,
        pub home: PathBuf,
        /// `<home>/.<app>/sample-app.worktrees`
        pub wt_root: PathBuf,
    }

    pub fn make_repos() -> Repos {
        make_repos_with(&[])
    }

    /// As `make_repos`, with extra branches (at the first commit) on origin.
    pub fn make_repos_with(branches: &[&str]) -> Repos {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let seed = root.join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "master"]);
        git(&seed, &["config", "user.email", "t@example.com"]);
        git(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("README.md"), "hi\n").unwrap();
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        for b in branches {
            git(&seed, &["branch", b]);
        }
        let origin = root.join("origin.git");
        git(&root, &["clone", "-q", "--bare", "seed", "origin.git"]);
        git(&root, &["clone", "-q", "origin.git", "sample-app"]);
        let clone = root.join("sample-app");
        git(&clone, &["config", "user.email", "t@example.com"]);
        git(&clone, &["config", "user.name", "T"]);
        let home = root.join("home");
        let wt_root = crate::paths::repo_worktrees_root(&clone, &home);
        Repos { _dir: dir, root, origin, clone, home, wt_root }
    }

    pub type Handler = Box<dyn Fn(&[String], &Path) -> Result<GitOut, String> + Send + Sync>;

    /// Answers from a closure and records every call.
    pub struct FakeGit {
        handler: Handler,
        pub calls: Mutex<Vec<(Vec<String>, PathBuf)>>,
    }

    pub fn out(stdout: &str, code: i32) -> Result<GitOut, String> {
        Ok(GitOut { stdout: stdout.into(), stderr: String::new(), code })
    }

    impl FakeGit {
        pub fn new(handler: impl Fn(&[String], &Path) -> Result<GitOut, String> + Send + Sync + 'static) -> Self {
            Self { handler: Box::new(handler), calls: Mutex::new(vec![]) }
        }

        pub fn args(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().iter().map(|(a, _)| a.clone()).collect()
        }
    }

    impl Git for FakeGit {
        async fn run(&self, args: &[&str], cwd: &Path, _opts: GitOpts) -> Result<GitOut, String> {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let r = (self.handler)(&args, cwd);
            self.calls.lock().unwrap().push((args, cwd.to_path_buf()));
            r
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn runs_git_and_returns_its_output_and_exit_code() {
        let out = SystemGit.run(&["--version"], Path::new("."), GitOpts::default()).await.unwrap();
        assert!(out.stdout.starts_with("git version") && out.code == 0);
        let failed = SystemGit.run(&["rev-parse", "--verify", "--quiet", "nope"], Path::new("."), GitOpts::allow_failure()).await.unwrap();
        assert_ne!(failed.code, 0);
        let err = SystemGit.run(&["rev-parse", "--verify", "nope-ref"], Path::new("."), GitOpts::default()).await.unwrap_err();
        assert!(err.starts_with("git rev-parse failed (exit "), "{err}");
    }

    #[tokio::test]
    async fn a_hung_git_is_killed_after_its_timeout_and_says_so_errors_are_redacted() {
        let started = Instant::now();
        let err = SystemGit.run(&["-c", "alias.hang=!sleep 5", "hang"], Path::new("."), GitOpts::timeout(Duration::from_millis(200))).await.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3));
        let err = SystemGit.run(&["ls-remote", "https://me:pw@127.0.0.1:1/x.git"], Path::new("."), GitOpts::timeout(Duration::from_secs(20))).await.unwrap_err();
        assert!(!err.contains("me:pw"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_working_directory_is_an_error_not_a_panic() {
        assert!(SystemGit.run(&["status"], Path::new("/definitely/not/here"), GitOpts::default()).await.is_err());
    }
}
