//! Runs `claude` headlessly and non-interactively (port of `core/claude.ts`).
//!
//! `--permission-mode bypassPermissions` alone does NOT skip tool-permission
//! prompts for Edit/Write/Bash (confirmed empirically: conflicts were silently
//! left unresolved because every Edit call was denied, visible only in Claude's
//! own `permission_denials`). The tool also has to be granted via
//! `--allowedTools`; `--tools` only controls which tools are *available*.
//!
//! This is still the trust boundary: it is only ever invoked with `cwd` pointing
//! at a disposable worktree that is never pushed until a human approves the
//! resulting diff in the extension panel. The guard hooks it wires in are
//! subcommands of this same binary (`<binary> guard <name>`), so there is no
//! guard script that could be missing; an empty `guard_binary` is refused by
//! `build_claude_args` (fail closed).

use crate::claude_args::{build_claude_args, protected_path_rules, protected_sandbox_paths, ClaudeArgsOptions, Policy, ProtectedDirs};
use crate::claude_stream::{StreamAccumulator, Throttle};
use crate::exec::{self, ExecError, RunOptions};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const PROGRESS_MIN_INTERVAL_MS: i64 = 1000;

/// Receives a progress label.
pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Receives the CLI's pid.
pub type SpawnFn = Arc<dyn Fn(u32) + Send + Sync>;

/// Where this install keeps things the run must be kept out of, and what to run.
#[derive(Debug, Clone)]
pub struct ClaudeEnv {
    pub state_dir: String,
    /// The companion's own install folder (never readable or writable by a run).
    pub companion_dir: String,
    pub home: String,
    /// This binary: what the guard hooks run (`<binary> guard ...`).
    pub guard_binary: String,
    /// The CLI to spawn; "claude" unless a test stands in for it.
    pub command: String,
}

impl ClaudeEnv {
    /// The running binary as the guard, `claude` from PATH.
    pub fn for_current_exe(state_dir: &str, home: &str) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let companion_dir = exe.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Self { state_dir: state_dir.into(), companion_dir, home: home.into(), guard_binary: exe.to_string_lossy().into_owned(), command: "claude".into() })
    }
}

pub struct ClaudeRunOptions {
    /// Directory the CLI runs in: the sandbox boundary. Every file edit and bash command Claude
    /// issues is scoped to this cwd (in practice, a disposable git worktree).
    pub cwd: String,
    pub prompt: String,
    /// Passed as `--model` only when set; unset means Claude Code's own default.
    pub model: Option<String>,
    /// A named policy: supplies the tools and says which guards are wired in.
    pub policy: Policy,
    /// Passed to `claude --session-id <uuid>` so a later phase can resume this exact transcript.
    pub session_id: Option<String>,
    /// Extra names appended to `--allowedTools` only (not `--tools`), e.g. `mcp__<server>` prefixes.
    pub extra_allowed_tools: Vec<String>,
    /// An explicit deny list that wins over allowedTools for known mutating MCP tools.
    pub disallowed_tools: Vec<String>,
    /// Extra paths the Bash sandbox may write to, beyond `cwd` (resolve-conflict passes the
    /// worktree's git dir, which lives in the main repo's `.git/worktrees/<id>`).
    pub sandbox_allow_write: Vec<String>,
    /// Extra paths the Bash sandbox may NOT write to, beyond what the policy adds itself.
    pub sandbox_deny_write: Vec<String>,
    /// Confines Edit/Write/MultiEdit/NotebookEdit (and Read/Glob/Grep for the worktree policies)
    /// to this one directory (a real path: resolve symlinks first), via the guard hooks.
    pub edit_root: Option<String>,
    /// Cancelling kills the CLI and its children and fails the run.
    pub cancel: Option<CancellationToken>,
    /// Called with the CLI's pid (its process-group id, when `cancel` is given) as soon as it
    /// spawns: the worktree features record it so a restarted service can refuse to touch a
    /// worktree an orphaned claude may still be writing.
    pub on_spawn: Option<SpawnFn>,
    /// Called with a short label ("Reading src/a.ts") as Claude works, at most about once a
    /// second. Providing it switches the run to stream-json output.
    pub on_progress: Option<ProgressFn>,
}

impl ClaudeRunOptions {
    pub fn new(cwd: &str, prompt: &str, policy: Policy) -> Self {
        Self {
            cwd: cwd.into(),
            prompt: prompt.into(),
            model: None,
            policy,
            session_id: None,
            extra_allowed_tools: vec![],
            disallowed_tools: vec![],
            sandbox_allow_write: vec![],
            sandbox_deny_write: vec![],
            edit_root: None,
            cancel: None,
            on_spawn: None,
            on_progress: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeRunResult {
    /// Claude's final text response.
    pub text: String,
    /// Raw stdout, kept for debugging when the JSON didn't parse as expected.
    pub raw: String,
    /// `claude`'s own session id for this run, falling back to the one the caller supplied.
    pub session_id: Option<String>,
    /// Names of tools the CLI refused to run; only set for stream-json runs.
    pub permission_denials: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum ClaudeError {
    /// The run couldn't be set up (a policy that needs a guard it can't wire).
    Setup(String),
    Exec(ExecError),
}

impl ClaudeError {
    pub fn is_aborted(&self) -> bool {
        matches!(self, ClaudeError::Exec(ExecError::Aborted))
    }
}

impl std::fmt::Display for ClaudeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeError::Setup(m) => f.write_str(m),
            ClaudeError::Exec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClaudeError {}

/// What features run Claude through, so their tests can stand in for it.
#[async_trait]
pub trait ClaudeRunner: Send + Sync {
    async fn run(&self, opts: ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError>;
}

/// The real thing: spawns the `claude` CLI.
pub struct SystemClaude {
    pub env: ClaudeEnv,
}

impl SystemClaude {
    pub fn new(env: ClaudeEnv) -> Self {
        Self { env }
    }

    /// The argv for a run (separate so the wiring is testable without spawning).
    pub fn args_for(&self, opts: &ClaudeRunOptions) -> Result<Vec<String>, ClaudeError> {
        let dirs = ProtectedDirs { state_dir: &self.env.state_dir, companion_dir: &self.env.companion_dir, home: &self.env.home };
        // `cwd` is passed through so build_claude_args adds it to the sandbox's deny-write itself
        // for any policy with `deny_cwd_write` (readOnly): a policy asks for this by setting the
        // flag on itself, with no policy-name check here.
        let o = ClaudeArgsOptions {
            prompt: opts.prompt.clone(),
            model: opts.model.clone(),
            policy: &opts.policy,
            session_id: opts.session_id.clone(),
            cwd: Some(opts.cwd.clone()),
            extra_allowed_tools: opts.extra_allowed_tools.clone(),
            disallowed_tools: opts.disallowed_tools.clone(),
            protected_rules: protected_path_rules(&dirs),
            sandbox_deny_read: protected_sandbox_paths(&dirs),
            sandbox_allow_write: opts.sandbox_allow_write.clone(),
            sandbox_deny_write: opts.sandbox_deny_write.clone(),
            edit_root: opts.edit_root.clone(),
            guard_binary: self.env.guard_binary.clone(),
            output_format: if opts.on_progress.is_some() { "stream-json".into() } else { "json".into() },
        };
        build_claude_args(&o).map_err(ClaudeError::Setup)
    }
}

#[async_trait]
impl ClaudeRunner for SystemClaude {
    async fn run(&self, opts: ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> {
        let args = self.args_for(&opts)?;
        let acc = opts.on_progress.as_ref().map(|_| Arc::new(std::sync::Mutex::new(StreamAccumulator::new(Some(&opts.cwd)))));
        let on_stdout_line: Option<exec::LineCallback> = match (&acc, &opts.on_progress) {
            (Some(acc), Some(progress)) => {
                let (acc, progress) = (acc.clone(), progress.clone());
                let mut throttle = Throttle::new(PROGRESS_MIN_INTERVAL_MS, || crate::job_files::now_ms() as i64);
                Some(Box::new(move |line: &str| {
                    let label = acc.lock().unwrap().feed(line);
                    if let Some(label) = label {
                        throttle.call(|| progress(&label));
                    }
                }))
            }
            _ => None,
        };
        let on_spawn = opts.on_spawn.clone().map(|f| -> Box<dyn Fn(u32) + Send + Sync> { Box::new(move |pid| f(pid)) });
        let result = exec::run(&self.env.command, &args, RunOptions { cwd: Some(PathBuf::from(&opts.cwd)), cancel: opts.cancel.clone(), on_spawn, on_stdout_line, ..Default::default() }).await.map_err(ClaudeError::Exec)?;

        if let Some(acc) = acc {
            let fin = acc.lock().unwrap().result();
            if fin.saw_result {
                if !fin.permission_denials.is_empty() {
                    eprintln!("[claude] {} tool call(s) were denied: {}", fin.permission_denials.len(), serde_json::to_string(&fin.permission_denials).unwrap_or_default());
                }
                return Ok(ClaudeRunResult {
                    text: if fin.text.is_empty() { result.stdout.clone() } else { fin.text },
                    raw: result.stdout,
                    session_id: fin.session_id.or(opts.session_id),
                    permission_denials: Some(fin.permission_denials),
                });
            }
            // No result event (an older CLI, or a cut-off run): fall through to the one-object
            // parse below, which degrades to the raw text.
        }

        Ok(parse_json_result(&result.stdout, opts.session_id))
    }
}

/// `claude --output-format json` returns a single result object whose final-answer field has
/// varied across CLI versions ("result" is the current one): fall back to the raw stdout if the
/// shape changes so a CLI upgrade degrades gracefully instead of failing.
fn parse_json_result(stdout: &str, session_id: Option<String>) -> ClaudeRunResult {
    let raw = stdout.to_string();
    let Ok(parsed) = serde_json::from_str::<Value>(stdout) else {
        return ClaudeRunResult { text: raw.clone(), raw, session_id, permission_denials: None };
    };
    let text = parsed["result"].as_str().or_else(|| parsed["text"].as_str()).map(str::to_string).unwrap_or_else(|| raw.clone());
    if let Some(denials) = parsed["permission_denials"].as_array().filter(|d| !d.is_empty()) {
        // Not necessarily fatal (a denied "run the test suite" is fine, a denied Edit on a
        // conflicted file is not): the caller decides by checking whether the markers are gone.
        let names: Vec<&str> = denials.iter().map(|d| d["tool_name"].as_str().unwrap_or("null")).collect();
        eprintln!("[claude] {} tool call(s) were denied: {}", denials.len(), serde_json::to_string(&names).unwrap_or_default());
    }
    let session_id = parsed["session_id"].as_str().map(str::to_string).or(session_id);
    ClaudeRunResult { text, raw, session_id, permission_denials: None }
}

/// A stand-in for tests of features: answers from a closure and records every call.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    type Handler = Box<dyn Fn(&ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> + Send + Sync>;

    pub struct FakeClaude {
        handler: Handler,
        pub calls: Mutex<Vec<(String, String, String)>>,
    }

    impl FakeClaude {
        pub fn new(handler: impl Fn(&ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> + Send + Sync + 'static) -> Self {
            Self { handler: Box::new(handler), calls: Mutex::new(vec![]) }
        }
        pub fn replying(text: &str) -> Self {
            let text = text.to_string();
            Self::new(move |o| Ok(ClaudeRunResult { text: text.clone(), raw: text.clone(), session_id: o.session_id.clone(), permission_denials: None }))
        }
    }

    #[async_trait]
    impl ClaudeRunner for FakeClaude {
        async fn run(&self, opts: ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> {
            self.calls.lock().unwrap().push((opts.cwd.clone(), opts.prompt.clone(), opts.policy.name.to_string()));
            (self.handler)(&opts)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// A shell script standing in for the `claude` CLI: prints `stdout`, records its argv to `argv.txt`.
    fn stand_in(dir: &std::path::Path, stdout: &str) -> ClaudeEnv {
        let script = dir.join("fake-claude");
        std::fs::write(&script, format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}/argv.txt'\ncat <<'EOF_OUT'\n{stdout}\nEOF_OUT\n", dir.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        ClaudeEnv { state_dir: "/state".into(), companion_dir: "/companion".into(), home: "/home/u".into(), guard_binary: "/bin/companion".into(), command: script.to_string_lossy().into_owned() }
    }

    fn opts(dir: &std::path::Path) -> ClaudeRunOptions {
        ClaudeRunOptions::new(&dir.to_string_lossy(), "do it", Policy::NO_TOOLS)
    }

    #[tokio::test]
    async fn a_json_result_gives_the_text_and_the_sessions_own_id_and_the_argv_carries_the_prompt() {
        let d = tempfile::tempdir().unwrap();
        let env = stand_in(d.path(), &json!({ "result": "all done", "session_id": "s-9", "permission_denials": [{ "tool_name": "Edit" }] }).to_string());
        let r = SystemClaude::new(env).run(ClaudeRunOptions { session_id: Some("mine".into()), ..opts(d.path()) }).await.unwrap();
        assert_eq!((r.text.as_str(), r.session_id.as_deref(), r.permission_denials), ("all done", Some("s-9"), None));
        let argv = std::fs::read_to_string(d.path().join("argv.txt")).unwrap();
        assert!(argv.starts_with("-p\ndo it\n"), "{argv}");
        assert!(argv.contains("--session-id\nmine\n") && argv.contains("json"), "{argv}");
    }

    #[test]
    fn unusual_replies_degrade_to_the_raw_text_and_the_callers_session_id() {
        let fall = |s: &str| parse_json_result(s, Some("mine".into()));
        assert_eq!(fall("plain words"), ClaudeRunResult { text: "plain words".into(), raw: "plain words".into(), session_id: Some("mine".into()), permission_denials: None });
        assert_eq!(fall("{\"text\":\"older\"}").text, "older");
        assert_eq!(fall("{\"other\":1}").text, "{\"other\":1}");
        assert_eq!(fall("null").text, "null");
        assert_eq!(fall("{\"result\":\"x\"}").session_id.as_deref(), Some("mine"));
        assert_eq!(parse_json_result("{\"result\":\"x\"}", None).session_id, None);
    }

    #[tokio::test]
    async fn progress_switches_to_stream_json_reports_labels_and_returns_the_result_events_text_and_denials() {
        let d = tempfile::tempdir().unwrap();
        let lines = [
            json!({ "type": "system", "subtype": "init", "session_id": "init-1" }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "name": "Read", "input": { "file_path": "a.ts" } }] } }),
            json!({ "type": "result", "result": "final answer", "session_id": "s-2", "permission_denials": [{ "tool_name": "Bash" }] }),
        ]
        .map(|l| l.to_string())
        .join("\n");
        let env = stand_in(d.path(), &lines);
        let labels = Arc::new(Mutex::new(vec![]));
        let l = labels.clone();
        let o = ClaudeRunOptions { on_progress: Some(Arc::new(move |s| l.lock().unwrap().push(s.to_string()))), ..opts(d.path()) };
        let r = SystemClaude::new(env).run(o).await.unwrap();
        assert_eq!((r.text.as_str(), r.session_id.as_deref(), r.permission_denials), ("final answer", Some("s-2"), Some(vec!["Bash".to_string()])));
        assert!(std::fs::read_to_string(d.path().join("argv.txt")).unwrap().contains("stream-json"));
        assert_eq!(labels.lock().unwrap().len(), 1, "throttled to one label in the first second: {:?}", labels.lock().unwrap());
        assert!(labels.lock().unwrap()[0].contains("a.ts"));
    }

    #[tokio::test]
    async fn a_stream_without_a_result_event_falls_back_to_the_raw_output() {
        let d = tempfile::tempdir().unwrap();
        let env = stand_in(d.path(), &json!({ "type": "system", "subtype": "init", "session_id": "i" }).to_string());
        let o = ClaudeRunOptions { on_progress: Some(Arc::new(|_| {})), ..opts(d.path()) };
        let r = SystemClaude::new(env).run(o).await.unwrap();
        assert!(r.text.contains("\"subtype\":\"init\""));
        assert_eq!(r.permission_denials, None);
    }

    #[tokio::test]
    async fn the_spawned_pid_is_reported_and_a_missing_cli_or_a_failing_one_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let pid = Arc::new(Mutex::new(None));
        let p = pid.clone();
        let o = ClaudeRunOptions { on_spawn: Some(Arc::new(move |n| *p.lock().unwrap() = Some(n))), ..opts(d.path()) };
        SystemClaude::new(stand_in(d.path(), "{}")).run(o).await.unwrap();
        assert!(pid.lock().unwrap().is_some());
        let mut missing = stand_in(d.path(), "{}");
        missing.command = "/no/such/claude".into();
        let e = SystemClaude::new(missing).run(opts(d.path())).await.unwrap_err();
        assert!(matches!(e, ClaudeError::Exec(ExecError::Spawn { .. })) && !e.is_aborted());
        let mut failing = stand_in(d.path(), "{}");
        failing.command = "false".into();
        assert!(matches!(SystemClaude::new(failing).run(opts(d.path())).await.unwrap_err(), ClaudeError::Exec(ExecError::Failed { .. })));
    }

    #[tokio::test]
    async fn cancelling_aborts_the_run() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("slow");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = stand_in(d.path(), "{}");
        env.command = script.to_string_lossy().into_owned();
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let handle = tokio::spawn(async move { SystemClaude::new(env).run(ClaudeRunOptions { cancel: Some(c), ..ClaudeRunOptions::new("/tmp", "x", Policy::NO_TOOLS) }).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        assert!(handle.await.unwrap().unwrap_err().is_aborted());
    }

    #[test]
    fn the_guards_are_wired_as_subcommands_of_the_binary_and_a_policy_that_needs_one_it_cannot_wire_is_refused() {
        let env = stand_in(std::path::Path::new("/tmp"), "{}");
        let claude = SystemClaude::new(env.clone());
        let mut o = ClaudeRunOptions::new("/wt", "x", Policy::WORKTREE_WRITE);
        o.edit_root = Some("/wt".into());
        let args = claude.args_for(&o).unwrap();
        let settings = args.iter().find(|a| a.contains("hooks")).expect("a --settings object");
        assert!(settings.contains("'/bin/companion' guard edit '/wt'"), "{settings}");
        assert!(settings.contains("/state") && settings.contains("/companion"), "the protected dirs are denied: {settings}");
        // No edit root for a worktree policy, or no guard binary at all: refused, never run unconfined.
        assert!(matches!(claude.args_for(&ClaudeRunOptions::new("/wt", "x", Policy::WORKTREE_WRITE)), Err(ClaudeError::Setup(_))));
        let no_binary = SystemClaude::new(ClaudeEnv { guard_binary: String::new(), ..env });
        assert!(matches!(no_binary.args_for(&ClaudeRunOptions::new("/wt", "x", Policy::READ_ONLY)), Err(ClaudeError::Setup(_))));
    }

    #[tokio::test]
    async fn the_fake_runner_records_calls() {
        let f = testing::FakeClaude::replying("hi");
        let r = f.run(ClaudeRunOptions::new("/w", "p", Policy::READ_ONLY)).await.unwrap();
        assert_eq!(r.text, "hi");
        assert_eq!(f.calls.lock().unwrap()[0], ("/w".to_string(), "p".to_string(), "readOnly".to_string()));
    }
}
