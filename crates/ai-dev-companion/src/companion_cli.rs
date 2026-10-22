//! The `companion` command line (port of `bin/companion.js`): what a distribution's `main` calls.
//! All the behaviour of the everyday commands is in `cli_run`; this routes first (the `guard` hooks
//! Claude invokes, then the commands that need the distribution: serve, setup, service,
//! review-worktrees, mcp-stdio) and supplies the real-world dependencies (`SystemCliDeps`).
//!
//! Two commands are advisory and never fail: `precheck` runs inside `git push` and `inbox --brief`
//! is the SessionStart hook, so both exit 0 whatever happens.

use crate::cli_run::{self, CliDeps};
use crate::git_cli::{GitOut, SystemGit};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_PORT: u64 = 8787;
const PRECHECK_GIT_LIMIT: Duration = Duration::from_millis(2500);
const GIT_LIMIT: Duration = Duration::from_secs(30);
const STDIN_LIMIT: usize = 64 * 1024;

/// What only the distribution can do. Every hook defaults to "not part of this build" (exit 64),
/// so a distribution implements the ones it has.
#[async_trait]
pub trait CliHooks: Send + Sync {
    type D: CliDeps;
    /// The dependencies of the everyday commands (`SystemCliDeps` for the real thing).
    fn deps(&self) -> &Self::D;
    /// `companion serve`: run the service in this process.
    async fn serve(&self) -> i32 {
        unavailable("serve")
    }
    /// `companion setup [args]`: the wizard.
    async fn setup(&self, _args: &[String]) -> i32 {
        unavailable("setup")
    }
    /// `companion service <install|uninstall|status>`.
    async fn service(&self, _sub: Option<&str>) -> i32 {
        unavailable("service")
    }
    /// `companion review-worktrees`: provision every configured repo's review worktree.
    async fn review_worktrees(&self) -> i32 {
        unavailable("review-worktrees")
    }
    /// `companion mcp-stdio`: the Claude Code plugin's MCP server over stdin/stdout.
    async fn mcp_stdio(&self) -> i32 {
        unavailable("mcp-stdio")
    }
}

fn unavailable(command: &str) -> i32 {
    eprintln!("companion: `{command}` isn't available in this build");
    64
}

/// Runs the command line (`args` are the process arguments after the program name) and returns
/// the exit code. Never panics on a bad command: an unknown one is the usage text and exit 2.
pub async fn run_cli<H: CliHooks>(args: &[String], hooks: &H) -> i32 {
    // The guard hooks Claude runs come first and must never reach anything else.
    if let Some(code) = crate::guard::cli::run_if_guard(args) {
        return code;
    }
    let rest = || args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("serve") => hooks.serve().await,
        Some("setup") => hooks.setup(rest()).await,
        Some("service") => hooks.service(args.get(1).map(String::as_str)).await,
        Some("review-worktrees") => hooks.review_worktrees().await,
        Some("mcp-stdio") => hooks.mcp_stdio().await,
        // The SessionStart hook is advisory in the same way `precheck` is (which `cli_run::main` covers).
        Some("inbox") if args.iter().any(|a| a == "--brief") => {
            cli_run::main(args, hooks.deps()).await;
            0
        }
        _ => cli_run::main(args, hooks.deps()).await,
    }
}

/// The port the service listens on, from the config (`8787` when unset).
pub fn port_of(config: &Value) -> u64 {
    config["port"].as_u64().filter(|p| (1..=65535).contains(p)).unwrap_or(DEFAULT_PORT)
}

/// The config in `companion_dir`, or `None` if there is none or it is unreadable.
pub fn read_config(companion_dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(companion_dir.join("config.json")).ok()?).ok()
}

/// Real dependencies for the everyday commands: the service on 127.0.0.1, `git`, `claude`, the
/// config and the MCP token of the installed copy at `companion_dir`.
pub struct SystemCliDeps {
    pub companion_dir: PathBuf,
    pub home: PathBuf,
    /// The pre-push check stops waiting after about 3 seconds and never holds a push up.
    pub precheck: bool,
    /// `inbox --brief`: silent on failure, never left waiting on a slow service.
    pub brief: bool,
    /// The program and arguments that run the doctor (the distribution's own binary, `doctor`).
    pub doctor_command: Vec<String>,
    pub work_units: crate::work_units::WorkUnits,
}

impl SystemCliDeps {
    /// `argv` is the command line, to tell the advisory commands apart.
    pub fn new(companion_dir: PathBuf, home: PathBuf, argv: &[String], doctor_command: Vec<String>) -> Self {
        let precheck = argv.first().map(String::as_str) == Some("precheck");
        let brief = argv.first().map(String::as_str) == Some("inbox") && argv.iter().any(|a| a == "--brief");
        let work_units = crate::work_units::WorkUnits::open(crate::work_units::default_file(&home));
        Self { companion_dir, home, precheck, brief, doctor_command, work_units }
    }

    fn port(&self) -> u64 {
        port_of(&read_config(&self.companion_dir).unwrap_or(Value::Null))
    }

    fn token(&self) -> Option<String> {
        let store = crate::credential_store::CredentialStore::new(self.companion_dir.join("credentials.enc"), crate::paths::state_dir(&self.home).join("credentials.key"));
        store.get_value_text("mcp.token").ok().flatten()
    }

    fn repo_paths(&self) -> Vec<String> {
        read_config(&self.companion_dir).and_then(|c| c["repos"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(String::from)).collect())).unwrap_or_default()
    }
}

/// A child with stdio inherited; `command` not found is the message Node printed, not a panic.
async fn run_inherited(command: &str, args: &[String], cwd: &str) -> i32 {
    match tokio::process::Command::new(command).args(args).current_dir(cwd).status().await {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("companion: `{command}` wasn't found on your PATH.");
            1
        }
        Err(e) => {
            eprintln!("companion: couldn't start {command}: {e}");
            1
        }
    }
}

#[async_trait]
impl CliDeps for SystemCliDeps {
    type G = SystemGit;

    fn worktree_git(&self) -> &SystemGit {
        &SystemGit
    }
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }
    fn now(&self) -> f64 {
        crate::job_files::now_ms()
    }
    fn home(&self) -> String {
        self.home.to_string_lossy().into_owned()
    }
    fn cwd(&self) -> String {
        std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into())
    }
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn out(&self, text: &str) {
        use std::io::Write;
        let mut o = std::io::stdout();
        let _ = o.write_all(text.as_bytes());
        let _ = o.flush();
    }
    fn err(&self, text: &str) {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
    }
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        let Some(token) = self.token() else { return Err("No MCP token is saved yet. Run `companion setup`, then try again.".into()) };
        let transport = crate::http::ReqwestTransport::new()?;
        let timeout = if self.precheck {
            Duration::from_millis(2500)
        } else if self.brief {
            Duration::from_millis(3000)
        } else {
            crate::mcp_client::DEFAULT_TIMEOUT
        };
        crate::mcp_client::call_tool(&transport, &format!("{}/mcp", self.base_url()), &token, name, args, timeout).await
    }
    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }
    fn cwd_allowed(&self, cwd: &str) -> bool {
        let sessions = crate::paths::state_dir(&self.home).join("sessions").to_string_lossy().into_owned();
        let repos = self.repo_paths();
        crate::session_cwd::session_cwd_allowed(Some(cwd), &crate::session_cwd::SessionCwdRules { repo_paths: &repos, sessions_root: Some(&sessions), ..Default::default() }, &self.home)
    }
    async fn run_claude(&self, args: Vec<String>, cwd: &str) -> i32 {
        run_inherited("claude", &args, cwd).await
    }
    async fn run_doctor(&self) -> i32 {
        let Some((program, rest)) = self.doctor_command.split_first() else { return 1 };
        run_inherited(program, rest, &self.companion_dir.to_string_lossy()).await
    }
    async fn git(&self, args: Vec<String>, cwd: &str, isolated: bool) -> GitOut {
        let base: BTreeMap<String, String> = std::env::vars().collect();
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(&args).current_dir(cwd).env_clear().envs(crate::git_hook::git_env(&base, isolated)).stdin(std::process::Stdio::null()).kill_on_drop(true);
        let limit = if self.precheck { PRECHECK_GIT_LIMIT } else { GIT_LIMIT };
        match tokio::time::timeout(limit, cmd.output()).await {
            Ok(Ok(o)) => GitOut { stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned(), code: o.status.code().unwrap_or(-1) },
            Ok(Err(e)) => GitOut { stdout: String::new(), stderr: e.to_string(), code: -1 },
            Err(_) => GitOut { stdout: String::new(), stderr: "timed out".into(), code: -1 },
        }
    }
    fn read_config(&self) -> Value {
        read_config(&self.companion_dir).unwrap_or_else(|| json!({}))
    }
    async fn read_stdin(&self) -> String {
        tokio::task::spawn_blocking(|| {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = std::io::stdin().take(STDIN_LIMIT as u64).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await
        .unwrap_or_default()
    }
    fn resolve(&self, cwd: &str, path: &str) -> String {
        if crate::node_path::is_absolute(path) {
            crate::node_path::resolve(path)
        } else {
            crate::node_path::resolve(&format!("{cwd}/{path}"))
        }
    }
    fn relative(&self, root: &str, path: &str) -> String {
        let root = root.trim_end_matches('/');
        match path.strip_prefix(root) {
            Some("") => String::new(),
            Some(rest) if rest.starts_with('/') => rest[1..].to_string(),
            _ => path.to_string(),
        }
    }
    fn mk_temp(&self) -> Result<String, String> {
        let dir = std::env::temp_dir().join(format!("companion-hook-probe-{}", crate::ids::uuid_v4()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(dir.to_string_lossy().into_owned())
    }
    fn rm_temp(&self, dir: &str) {
        let _ = std::fs::remove_dir_all(dir);
    }
    fn brief_timeout(&self) -> Duration {
        Duration::from_millis(3000)
    }
    fn work_units(&self) -> Option<&crate::work_units::WorkUnits> {
        Some(&self.work_units)
    }
    fn job_worktrees_root(&self) -> Option<String> {
        Some(crate::paths::state_dir(&self.home).join("worktrees").to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records which hook ran; the everyday commands go to `Inner`, a stand-in for `cli_run`'s deps.
    struct Hooks {
        inner: SystemCliDeps,
        calls: Mutex<Vec<String>>,
    }

    fn hooks(argv: &[&str]) -> (Hooks, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let inner = SystemCliDeps::new(d.path().join("app"), d.path().join("home"), &argv, vec!["true".into()]);
        (Hooks { inner, calls: Mutex::new(vec![]) }, d)
    }

    #[async_trait]
    impl CliHooks for Hooks {
        type D = SystemCliDeps;
        fn deps(&self) -> &SystemCliDeps {
            &self.inner
        }
        async fn serve(&self) -> i32 {
            self.calls.lock().unwrap().push("serve".into());
            0
        }
        async fn setup(&self, args: &[String]) -> i32 {
            self.calls.lock().unwrap().push(format!("setup {}", args.join(" ")));
            3
        }
        async fn service(&self, sub: Option<&str>) -> i32 {
            self.calls.lock().unwrap().push(format!("service {}", sub.unwrap_or("-")));
            4
        }
        async fn review_worktrees(&self) -> i32 {
            self.calls.lock().unwrap().push("review-worktrees".into());
            5
        }
        async fn mcp_stdio(&self) -> i32 {
            self.calls.lock().unwrap().push("mcp-stdio".into());
            6
        }
    }

    async fn run(argv: &[&str]) -> (i32, Vec<String>) {
        let (h, _d) = hooks(argv);
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let code = run_cli(&args, &h).await;
        let calls = h.calls.lock().unwrap().clone();
        (code, calls)
    }

    #[tokio::test]
    async fn the_distribution_commands_go_to_their_hooks_with_their_arguments() {
        assert_eq!(run(&["serve"]).await, (0, vec!["serve".to_string()]));
        assert_eq!(run(&["setup", "--yes"]).await, (3, vec!["setup --yes".to_string()]));
        assert_eq!(run(&["service", "status"]).await, (4, vec!["service status".to_string()]));
        assert_eq!(run(&["service"]).await, (4, vec!["service -".to_string()]));
        assert_eq!(run(&["review-worktrees"]).await, (5, vec!["review-worktrees".to_string()]));
        assert_eq!(run(&["mcp-stdio"]).await, (6, vec!["mcp-stdio".to_string()]));
    }

    #[tokio::test]
    async fn an_unknown_command_is_the_usage_text_and_exit_2_and_touches_no_hook() {
        let (code, calls) = run(&["frobnicate"]).await;
        assert_eq!(code, 2);
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn the_advisory_commands_exit_zero_even_when_the_service_is_down() {
        // Nothing listens on the config-less default port path: no token is saved, so both fail inside and still exit 0.
        assert_eq!(run(&["precheck"]).await.0, 0);
        assert_eq!(run(&["inbox", "--brief"]).await.0, 0);
    }

    #[tokio::test]
    async fn a_hook_a_distribution_leaves_out_is_unavailable_with_exit_64() {
        struct Bare(SystemCliDeps);
        #[async_trait]
        impl CliHooks for Bare {
            type D = SystemCliDeps;
            fn deps(&self) -> &SystemCliDeps {
                &self.0
            }
        }
        let d = tempfile::tempdir().unwrap();
        let b = Bare(SystemCliDeps::new(d.path().join("app"), d.path().join("home"), &[], vec![]));
        for cmd in ["serve", "setup", "service", "review-worktrees", "mcp-stdio"] {
            assert_eq!(run_cli(&[cmd.to_string()], &b).await, 64, "{cmd}");
        }
    }

    #[test]
    fn the_port_comes_from_the_config_and_falls_back_to_8787() {
        assert_eq!(port_of(&json!({ "port": 9001 })), 9001);
        for bad in [json!({}), json!({ "port": 0 }), json!({ "port": 70000 }), json!({ "port": "x" }), Value::Null] {
            assert_eq!(port_of(&bad), 8787, "{bad}");
        }
    }

    #[test]
    fn the_advisory_flags_are_read_from_the_command_line() {
        let d = tempfile::tempdir().unwrap();
        let mk = |a: &[&str]| SystemCliDeps::new(d.path().into(), d.path().into(), &a.iter().map(|s| s.to_string()).collect::<Vec<_>>(), vec![]);
        assert!(mk(&["precheck"]).precheck && !mk(&["precheck"]).brief);
        assert!(mk(&["inbox", "--brief"]).brief && !mk(&["inbox"]).brief && !mk(&["status", "--brief"]).brief);
    }

    #[test]
    fn paths_resolve_against_the_cwd_and_relative_paths_are_under_a_root_only() {
        let d = tempfile::tempdir().unwrap();
        let s = SystemCliDeps::new(d.path().into(), d.path().into(), &[], vec![]);
        assert_eq!(s.resolve("/w", "a/../b"), "/w/b");
        assert_eq!(s.resolve("/w", "/x/y"), "/x/y");
        assert_eq!(s.relative("/root/", "/root/a/b"), "a/b");
        assert_eq!(s.relative("/root", "/rooty/a"), "/rooty/a");
        assert_eq!(s.relative("/root", "/root"), "");
    }

    #[tokio::test]
    async fn the_real_git_runs_with_an_isolated_config_and_reports_a_missing_repo_as_a_failure_code() {
        let d = tempfile::tempdir().unwrap();
        let s = SystemCliDeps::new(d.path().into(), d.path().into(), &[], vec![]);
        let out = s.git(vec!["rev-parse".into(), "--git-dir".into()], &d.path().to_string_lossy(), true).await;
        assert_ne!(out.code, 0);
        let v = s.git(vec!["--version".into()], &d.path().to_string_lossy(), false).await;
        assert!(v.code == 0 && v.stdout.starts_with("git version"));
    }

    #[tokio::test]
    async fn with_no_token_saved_a_tool_call_says_to_run_setup() {
        let d = tempfile::tempdir().unwrap();
        let s = SystemCliDeps::new(d.path().join("app"), d.path().join("home"), &[], vec![]);
        assert!(s.call_tool("list_jobs", json!({})).await.unwrap_err().contains("No MCP token"));
    }

    #[tokio::test]
    async fn a_missing_program_is_a_message_and_exit_1_not_a_panic() {
        assert_eq!(run_inherited("definitely-not-a-real-program-xyz", &[], "/").await, 1);
    }
}
