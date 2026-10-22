//! Running external commands (port of `core/exec.ts`).
//!
//! Always an argv array, never a shell string, so branch names, PR ids or
//! anything else that ultimately traces back to the extension's request body
//! can never be interpreted as shell syntax.

use crate::js_text::js_trim;
use crate::line_splitter::LineSplitter;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    /// The exit code; -1 when the process was killed by a signal.
    pub code: i32,
}

#[derive(Debug)]
pub enum ExecError {
    /// The command couldn't be started (not found, not executable, bad cwd).
    Spawn { command: String, source: std::io::Error },
    /// It ran and exited non-zero (and `allow_failure` was off).
    Failed { command: String, args: Vec<String>, result: ExecResult },
    /// The caller's cancellation token fired. A killed command's result is
    /// meaningless, so this is an error even with `allow_failure`.
    Aborted,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Spawn { command, source } => write!(f, "spawn {command}: {source}"),
            ExecError::Failed { command, args, result } => {
                let detail = if js_trim(&result.stderr).is_empty() { js_trim(&result.stdout) } else { js_trim(&result.stderr) };
                write!(f, "{command} {} exited with {}: {detail}", args.join(" "), result.code)
            }
            ExecError::Aborted => write!(f, "aborted"),
        }
    }
}

impl std::error::Error for ExecError {}

/// Process-group ids (= pids, since each is its group's leader) of every
/// detached child still running. Tracked so a shutdown can take them down with
/// it: a detached child is in its own group, so a signal to this process alone
/// never reaches it, and a headless claude that outlived the service would keep
/// writing a worktree the restarted service then treats as settled.
static DETACHED: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn track(pid: u32, add: bool) {
    let mut g = DETACHED.lock().unwrap();
    let set = g.get_or_insert_with(HashSet::new);
    if add { set.insert(pid) } else { set.remove(&pid) };
}

#[cfg(unix)]
fn signal_group(pgid: u32, sig: i32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else { return false };
    // SAFETY: signalling a process group we created; failures (ESRCH) are ignored by callers.
    unsafe { libc::kill(-pgid, sig) == 0 }
}

#[cfg(not(unix))]
fn signal_group(_pgid: u32, _sig: i32) -> bool {
    false
}

/// Sends `sig` (`libc::SIGTERM` by default) to every tracked detached child's
/// whole process group (claude plus whatever its Bash tool started). ESRCH (the
/// group already exited between its exit event and now) is ignored, as is
/// anything else, since this runs on the way out and must not fail there.
pub fn kill_detached_children(sig: i32) {
    let pids: Vec<u32> = DETACHED.lock().unwrap().as_ref().map(|s| s.iter().copied().collect()).unwrap_or_default();
    for pid in pids {
        signal_group(pid, sig);
    }
}

/// Whether `pid` is a tracked, still-running detached child.
pub fn is_detached(pid: u32) -> bool {
    DETACHED.lock().unwrap().as_ref().is_some_and(|s| s.contains(&pid))
}

/// How many detached children haven't exited yet: the server's shutdown
/// handler waits (bounded) for this to reach 0 after [`kill_detached_children`].
pub fn detached_child_count() -> usize {
    DETACHED.lock().unwrap().as_ref().map_or(0, HashSet::len)
}

/// A per-line stdout listener.
pub type LineCallback = Box<dyn FnMut(&str) + Send>;

#[derive(Default)]
pub struct RunOptions {
    pub cwd: Option<PathBuf>,
    /// Written to the child's stdin, which is then closed.
    pub input: Option<String>,
    pub allow_failure: bool,
    /// With a token, the child gets its own process group so cancelling can
    /// SIGTERM everything it started too (claude's Bash tool runs its own
    /// subprocesses).
    pub cancel: Option<CancellationToken>,
    /// Called with the child's pid right after it spawns. For a detached child
    /// (one given a token) that's also its process-group id. The worktree
    /// features persist it, so after a crash or `kill -9` (when
    /// [`kill_detached_children`] never got to run) the restarted service can
    /// tell whether that claude is still alive.
    pub on_spawn: Option<Box<dyn Fn(u32) + Send + Sync>>,
    /// Called with each complete line of stdout as it arrives (the whole output
    /// is still buffered and returned). Used for streamed progress.
    pub on_stdout_line: Option<LineCallback>,
}

/// Incremental UTF-8 decoding: a multi-byte character split across two reads is
/// held back until its remaining bytes arrive (what `StringDecoder` does).
#[derive(Default)]
struct Utf8Chunker {
    pending: Vec<u8>,
}

impl Utf8Chunker {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap_or(""));
                    match e.error_len() {
                        // An incomplete character at the end: wait for more bytes.
                        None => {
                            self.pending.drain(..valid);
                            return out;
                        }
                        Some(n) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + n);
                        }
                    }
                }
            }
        }
    }

    fn finish(&mut self) -> String {
        let rest = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        rest
    }
}

pub async fn run(command: &str, args: &[String], opts: RunOptions) -> Result<ExecResult, ExecError> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = &opts.cwd {
        cmd.current_dir(cwd);
    }
    let detached = opts.cancel.is_some();
    #[cfg(unix)]
    if detached {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|source| ExecError::Spawn { command: command.to_string(), source })?;
    let pid = child.id();
    if let (true, Some(pid)) = (detached, pid) {
        track(pid, true);
    }
    if let (Some(pid), Some(cb)) = (pid, &opts.on_spawn) {
        cb(pid);
    }

    // stdin: write the input, then close it so the child sees EOF.
    let mut stdin = child.stdin.take();
    let input = opts.input.clone();
    let writer = tokio::spawn(async move {
        if let (Some(si), Some(input)) = (stdin.as_mut(), input) {
            let _ = si.write_all(input.as_bytes()).await;
        }
        drop(stdin);
    });

    let mut out_pipe = child.stdout.take().expect("stdout is piped");
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let mut on_line = opts.on_stdout_line;
    let out_task = tokio::spawn(async move {
        let mut all: Vec<u8> = Vec::new();
        let mut splitter = on_line.as_mut().map(|cb| LineSplitter::new(move |l: &str| cb(l)));
        let mut chunker = Utf8Chunker::default();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match out_pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    all.extend_from_slice(&buf[..n]);
                    if let Some(s) = splitter.as_mut() {
                        s.write(&chunker.push(&buf[..n]));
                    }
                }
            }
        }
        if let Some(s) = splitter.as_mut() {
            s.write(&chunker.finish());
            s.flush();
        }
        all
    });
    let err_task = tokio::spawn(async move {
        let mut all = Vec::new();
        let _ = err_pipe.read_to_end(&mut all).await;
        all
    });

    let mut aborted = false;
    let status = match &opts.cancel {
        Some(token) => {
            tokio::select! {
                s = child.wait() => s,
                () = token.cancelled() => {
                    aborted = true;
                    if !pid.is_some_and(|p| signal_group(p, libc::SIGTERM)) {
                        let _ = child.start_kill();
                    }
                    child.wait().await
                }
            }
        }
        None => child.wait().await,
    };
    if let (true, Some(pid)) = (detached, pid) {
        track(pid, false);
    }
    let _ = writer.await;
    let stdout_bytes = out_task.await.unwrap_or_default();
    let stderr_bytes = err_task.await.unwrap_or_default();
    if aborted || opts.cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Err(ExecError::Aborted);
    }
    let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
    let result = ExecResult {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        code,
    };
    if result.code != 0 && !opts.allow_failure {
        return Err(ExecError::Failed { command: command.to_string(), args: args.to_vec(), result });
    }
    Ok(result)
}

/// `git <args>` in `cwd`.
pub async fn git(args: &[&str], cwd: &std::path::Path, allow_failure: bool, cancel: Option<CancellationToken>) -> Result<ExecResult, ExecError> {
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    run("git", &argv, RunOptions { cwd: Some(cwd.to_path_buf()), allow_failure, cancel, ..Default::default() }).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> (String, Vec<String>) {
        ("/bin/sh".into(), vec!["-c".into(), script.into()])
    }

    async fn run_sh(script: &str, opts: RunOptions) -> Result<ExecResult, ExecError> {
        let (c, a) = sh(script);
        run(&c, &a, opts).await
    }

    #[tokio::test]
    async fn captures_stdout_stderr_and_the_exit_code() {
        let r = run_sh("echo out; echo err >&2", RunOptions::default()).await.unwrap();
        assert_eq!(r, ExecResult { stdout: "out\n".into(), stderr: "err\n".into(), code: 0 });
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_an_error_naming_the_command_and_stderr_unless_allow_failure() {
        let err = run_sh("echo bad >&2; exit 3", RunOptions::default()).await.unwrap_err();
        match &err {
            ExecError::Failed { result, .. } => assert_eq!(result.code, 3),
            other => panic!("{other:?}"),
        }
        assert_eq!(err.to_string(), "/bin/sh -c echo bad >&2; exit 3 exited with 3: bad");
        let ok = run_sh("echo bad >&2; exit 3", RunOptions { allow_failure: true, ..Default::default() }).await.unwrap();
        assert_eq!(ok.code, 3);
        // With nothing on stderr the message falls back to stdout.
        let err = run_sh("echo only-stdout; exit 1", RunOptions::default()).await.unwrap_err();
        assert!(err.to_string().ends_with("exited with 1: only-stdout"));
    }

    #[tokio::test]
    async fn arguments_are_never_interpreted_by_a_shell() {
        let r = run("echo", &["$HOME; rm -rf x `id`".to_string()], RunOptions::default()).await.unwrap();
        assert_eq!(r.stdout, "$HOME; rm -rf x `id`\n");
    }

    #[tokio::test]
    async fn input_is_piped_to_stdin_and_cwd_is_honoured() {
        let r = run("cat", &[], RunOptions { input: Some("héllo\nworld".into()), ..Default::default() }).await.unwrap();
        assert_eq!(r.stdout, "héllo\nworld");
        let dir = tempfile::tempdir().unwrap();
        let r = run("pwd", &[], RunOptions { cwd: Some(dir.path().to_path_buf()), ..Default::default() }).await.unwrap();
        assert_eq!(std::fs::canonicalize(r.stdout.trim()).unwrap(), std::fs::canonicalize(dir.path()).unwrap());
        // A command that doesn't read its stdin doesn't hang the writer.
        let r = run_sh("echo done", RunOptions { input: Some("x".repeat(1_000_000)), ..Default::default() }).await.unwrap();
        assert_eq!(r.stdout, "done\n");
    }

    #[tokio::test]
    async fn a_missing_command_or_directory_is_a_spawn_error() {
        assert!(matches!(run("definitely-not-a-command-xyz", &[], RunOptions::default()).await, Err(ExecError::Spawn { .. })));
        let bad_cwd = RunOptions { cwd: Some(PathBuf::from("/no/such/dir")), ..Default::default() };
        assert!(matches!(run("echo", &[], bad_cwd).await, Err(ExecError::Spawn { .. })));
    }

    #[tokio::test]
    async fn stdout_lines_stream_as_they_arrive_while_the_whole_output_is_still_returned() {
        let lines = Arc::new(StdMutex::new(Vec::<String>::new()));
        let l = lines.clone();
        let opts = RunOptions { on_stdout_line: Some(Box::new(move |line| l.lock().unwrap().push(line.to_string()))), ..Default::default() };
        let r = run_sh("printf a; sleep 0.1; printf 'b\\r\\n\\nc\\n'; printf tail", opts).await.unwrap();
        assert_eq!(*lines.lock().unwrap(), ["ab", "c", "tail"]);
        assert_eq!(r.stdout, "ab\r\n\nc\ntail");
    }

    #[tokio::test]
    async fn a_multibyte_character_split_across_reads_is_decoded_whole() {
        let lines = Arc::new(StdMutex::new(Vec::<String>::new()));
        let l = lines.clone();
        let opts = RunOptions { on_stdout_line: Some(Box::new(move |line| l.lock().unwrap().push(line.to_string()))), ..Default::default() };
        run_sh("printf '\\303'; sleep 0.1; printf '\\251\\n'", opts).await.unwrap();
        assert_eq!(*lines.lock().unwrap(), ["\u{e9}"]);
        let mut c = Utf8Chunker::default();
        assert_eq!(c.push(&[0xe2, 0x82]), "");
        assert_eq!(c.push(&[0xac, b'x']), "\u{20ac}x");
        assert_eq!(c.push(&[0xff, b'y']), "\u{fffd}y");
        assert_eq!(c.push(&[0xc3]), "");
        assert_eq!(c.finish(), "\u{fffd}");
    }

    #[tokio::test]
    async fn on_spawn_reports_the_pid_and_a_token_makes_the_child_a_tracked_group_leader() {
        let pid = Arc::new(StdMutex::new(None));
        let p = pid.clone();
        let token = CancellationToken::new();
        let opts = RunOptions { cancel: Some(token.clone()), on_spawn: Some(Box::new(move |x| *p.lock().unwrap() = Some(x))), ..Default::default() };
        let handle = tokio::spawn(run_sh("sleep 5", opts));
        tokio::time::sleep(Duration::from_millis(150)).await;
        let pgid = pid.lock().unwrap().expect("spawned");
        assert!(is_detached(pgid) && detached_child_count() >= 1);
        assert!(crate::process_group::is_process_group_alive(i64::from(pgid)), "the child leads its own group");
        token.cancel();
        let started = Instant::now();
        assert!(matches!(handle.await.unwrap(), Err(ExecError::Aborted)));
        assert!(started.elapsed() < Duration::from_secs(3), "cancelling doesn't wait for the sleep");
        assert!(!is_detached(pgid));
        assert!(!crate::process_group::is_process_group_alive(i64::from(pgid)));
    }

    #[tokio::test]
    async fn cancelling_takes_down_what_the_child_started_too_even_with_allow_failure() {
        let marker = tempfile::tempdir().unwrap().keep();
        let file = marker.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {}; wait", file.display());
        let token = CancellationToken::new();
        let t = token.clone();
        let handle = tokio::spawn(async move { run_sh(&script, RunOptions { cancel: Some(t), allow_failure: true, ..Default::default() }).await });
        let mut grandchild: Option<i32> = None;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(n) = std::fs::read_to_string(&file).ok().and_then(|t| t.trim().parse().ok()) {
                grandchild = Some(n);
                break;
            }
        }
        let grandchild = grandchild.expect("the grandchild wrote its pid");
        token.cancel();
        assert!(matches!(handle.await.unwrap(), Err(ExecError::Aborted)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        // SAFETY: signal 0 only probes.
        let alive = unsafe { libc::kill(grandchild, 0) } == 0;
        assert!(!alive, "the grandchild was in the group and got the SIGTERM");
        let _ = std::fs::remove_dir_all(marker);
    }

    #[tokio::test]
    async fn an_already_cancelled_token_aborts_without_a_result() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(run_sh("sleep 5", RunOptions { cancel: Some(token), ..Default::default() }).await, Err(ExecError::Aborted)));
    }

    #[tokio::test]
    async fn git_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        git(&["init", "-q"], dir.path(), false, None).await.unwrap();
        let r = git(&["rev-parse", "--is-inside-work-tree"], dir.path(), false, None).await.unwrap();
        assert_eq!(r.stdout.trim(), "true");
        assert!(git(&["no-such-subcommand"], dir.path(), false, None).await.is_err());
        assert_eq!(git(&["no-such-subcommand"], dir.path(), true, None).await.unwrap().code, 1);
    }
}
