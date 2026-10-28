//! Opening a folder in an editor and starting an AI review there (port of
//! `core/editor.ts`). macOS only, as the terminal launcher is.
//!
//! - Claude Code runs in a new terminal window.
//! - Cursor opens the folder, then a prompt deeplink starts the review in its Agent window.
//! - VS Code opens the folder, then starts the review in its Copilot Chat.

use crate::cursor_deeplink::build_cursor_prompt_url;
use crate::exec::{self, ExecError, RunOptions};
use crate::terminal::{HeaderSegment, Terminal, TerminalOptions};
use crate::terminal_focus::TerminalLocation;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorId {
    Vscode,
    Cursor,
    ClaudeCode,
}

impl EditorId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vscode => "vscode",
            Self::Cursor => "cursor",
            Self::ClaudeCode => "claude-code",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "vscode" => Some(Self::Vscode),
            "cursor" => Some(Self::Cursor),
            "claude-code" => Some(Self::ClaudeCode),
            _ => None,
        }
    }

    fn app_name(self) -> &'static str {
        match self {
            Self::Vscode => "Visual Studio Code",
            Self::Cursor => "Cursor",
            Self::ClaudeCode => "Claude Code",
        }
    }
}

/// How long Cursor gets to open the worktree's window before the deeplink is sent, so the link's
/// `workspace` match finds that window instead of landing in whichever one was frontmost.
pub const CURSOR_WINDOW_SETTLE: Duration = Duration::from_millis(4000);
const VSCODE_BUNDLED_CLI: &str = "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLaunch {
    /// Whether the editor's IDE window was opened.
    pub opened_editor: bool,
    pub review_started: bool,
    pub review_note: String,
    /// The terminal a terminal-based review opened in, if it said.
    pub terminal: Option<TerminalLocation>,
}

/// Claude Code only: `new_id` names the new session (so it can be resumed later), `resume_id`
/// reopens that session instead of starting one.
#[derive(Debug, Clone, Default)]
pub struct ReviewSession {
    pub new_id: Option<String>,
    pub resume_id: Option<String>,
}

/// What opens things: the commands it runs and the places it looks, injectable for tests.
#[derive(Debug, Clone)]
pub struct Editors {
    pub terminal: Terminal,
    /// `open`, the macOS launcher.
    pub open_command: String,
    pub vscode_bundled_cli: PathBuf,
    /// The `code` command looked up on PATH when the bundled one isn't there.
    pub code_command: String,
    pub cursor_window_settle: Duration,
    pub require_macos: bool,
}

impl Default for Editors {
    fn default() -> Self {
        Self { terminal: Terminal::default(), open_command: "open".into(), vscode_bundled_cli: VSCODE_BUNDLED_CLI.into(), code_command: "code".into(), cursor_window_settle: CURSOR_WINDOW_SETTLE, require_macos: true }
    }
}

fn not_installed(e: &ExecError) -> bool {
    matches!(e, ExecError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
}

fn review_failure_note(err: &ExecError, editor: EditorId) -> String {
    let reason = if not_installed(err) {
        if editor == EditorId::Cursor { "Cursor isn't installed".to_string() } else { "the `code` command isn't installed".to_string() }
    } else {
        err.to_string()
    };
    format!("Couldn't start the automatic review: {reason}.")
}

impl Editors {
    async fn run(&self, command: &str, args: &[&str], cwd: Option<&Path>) -> Result<(), ExecError> {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        exec::run(command, &argv, RunOptions { cwd: cwd.map(Path::to_path_buf), ..Default::default() }).await.map(|_| ())
    }

    /// Opens `dir` in the given editor. "claude-code" is a new terminal window running `claude`
    /// (there's no URL scheme for that); "vscode"/"cursor" shell out to `open -a`, which needs no
    /// scheme registration and covers VS Code.
    pub async fn open_in_editor(&self, dir: &str, editor: EditorId) -> Result<(), String> {
        if editor == EditorId::ClaudeCode {
            self.terminal.open_claude_code(dir, &[], &[], &TerminalOptions::default()).await?;
            return Ok(());
        }
        if self.require_macos && !cfg!(target_os = "macos") {
            return Err(format!("Opening {} is only implemented for macOS.", editor.app_name()));
        }
        self.run(&self.open_command, &["-a", editor.app_name(), dir], None).await.map_err(|e| e.to_string())
    }

    /// Starts an AI review of `dir` on `prompt` (Claude Code uses `permission_mode`, the profile's
    /// for review-in-editor). `header` is printed at the top of the terminal for the terminal-based
    /// reviews, and `options` titles its tab and names its status file. Fails only if nothing at
    /// all could be opened; a review that fails to start is reported in `review_note`.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_for_review(&self, dir: &str, editor: EditorId, prompt: &str, permission_mode: &str, header: &[Vec<HeaderSegment>], options: &TerminalOptions, session: &ReviewSession) -> Result<ReviewLaunch, String> {
        match editor {
            EditorId::ClaudeCode => {
                let mode = ["--permission-mode".to_string(), permission_mode.to_string()];
                if let Some(resume) = session.resume_id.as_deref().filter(|r| !r.is_empty()) {
                    let args = ["--resume".to_string(), resume.to_string(), mode[0].clone(), mode[1].clone()];
                    let terminal = self.terminal.open_claude_code(dir, &args, header, options).await?;
                    return Ok(ReviewLaunch { opened_editor: false, review_started: true, review_note: "Reopened the last review's Claude Code session in a new terminal window.".into(), terminal });
                }
                let mut args: Vec<String> = vec![];
                if let Some(id) = session.new_id.as_deref().filter(|i| !i.is_empty()) {
                    args.extend(["--session-id".to_string(), id.to_string()]);
                }
                args.extend(mode);
                args.push(prompt.to_string());
                let terminal = self.terminal.open_claude_code(dir, &args, header, options).await?;
                Ok(ReviewLaunch { opened_editor: false, review_started: true, review_note: "Claude Code is reviewing it in a new terminal window.".into(), terminal })
            }
            EditorId::Cursor => match self.open_cursor_review(dir, prompt).await {
                Ok(launch) => Ok(launch),
                Err(e) => Ok(ReviewLaunch { opened_editor: true, review_started: false, review_note: review_failure_note(&e, EditorId::Cursor), terminal: None }),
            },
            EditorId::Vscode => {
                self.open_in_editor(dir, editor).await?;
                let cli = if self.vscode_bundled_cli.exists() { self.vscode_bundled_cli.to_string_lossy().into_owned() } else { self.code_command.clone() };
                // `code chat` targets the window for its working directory.
                match self.run(&cli, &["chat", "--mode", "ask", prompt], Some(Path::new(dir))).await {
                    Ok(()) => Ok(ReviewLaunch { opened_editor: true, review_started: true, review_note: "Copilot Chat is reviewing it in Ask mode (read-only).".into(), terminal: None }),
                    Err(e) => Ok(ReviewLaunch { opened_editor: true, review_started: false, review_note: review_failure_note(&e, editor), terminal: None }),
                }
            }
        }
    }

    /// Opens `dir` in Cursor, then sends its prompt deeplink so the review is a new Ask-mode chat
    /// in the IDE's Agent window. Cursor always shows a "Create chat with prompt" confirmation, so
    /// the review starts when the user clicks Create Chat; the chat can't be tracked from here.
    /// Cursor rejects prompts over ~10k encoded characters, so longer ones fail here instead of
    /// being silently refused inside the IDE.
    async fn open_cursor_review(&self, dir: &str, prompt: &str) -> Result<ReviewLaunch, ExecError> {
        let other = |m: String| ExecError::Spawn { command: "cursor".into(), source: std::io::Error::other(m) };
        if self.require_macos && !cfg!(target_os = "macos") {
            return Err(other("Opening Cursor is only implemented for macOS.".into()));
        }
        let url = build_cursor_prompt_url(dir, prompt).map_err(other)?;
        self.open_in_editor(dir, EditorId::Cursor).await.map_err(other)?;
        tokio::time::sleep(self.cursor_window_settle).await;
        self.run(&self.open_command, &[&url], None).await?;
        Ok(ReviewLaunch {
            opened_editor: true,
            review_started: true,
            review_note: "Cursor opened a new Ask-mode (read-only) chat with the review prompt in its Agent window. Click Create Chat in Cursor to start it.".into(),
            terminal: None,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Fx {
        _d: tempfile::TempDir,
        root: PathBuf,
        editors: Editors,
    }

    fn script(path: &Path, body: &str) -> String {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// `open` and `code` stand-ins that log their argv (one call per line) into `calls.txt`.
    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let calls = root.join("calls.txt");
        let open = script(&root.join("open"), &format!("echo \"open $*\" >> '{}'", calls.display()));
        let osa = script(&root.join("osascript"), "echo ''");
        let tmp = root.join("tmp");
        fs::create_dir(&tmp).unwrap();
        let editors = Editors {
            terminal: Terminal { osascript: osa, iterm_app: "/no/iterm".into(), tmp_dir: tmp, require_macos: false },
            open_command: open,
            vscode_bundled_cli: root.join("bundled-code"),
            // Never the real `code`: a test that falls back to it would launch the real VS Code.
            code_command: root.join("no-such-code").to_string_lossy().into_owned(),
            cursor_window_settle: Duration::from_millis(1),
            require_macos: false,
        };
        Fx { _d: d, root, editors }
    }

    fn calls(f: &Fx) -> Vec<String> {
        fs::read_to_string(f.root.join("calls.txt")).map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
    }

    #[test]
    fn editor_ids_round_trip() {
        for id in ["vscode", "cursor", "claude-code"] {
            assert_eq!(EditorId::parse(id).unwrap().as_str(), id);
        }
        assert_eq!(EditorId::parse("emacs"), None);
    }

    #[tokio::test]
    async fn vscode_and_cursor_open_by_app_name_and_claude_code_opens_a_terminal() {
        let f = fx();
        f.editors.open_in_editor("/work/app", EditorId::Vscode).await.unwrap();
        f.editors.open_in_editor("/work/app", EditorId::Cursor).await.unwrap();
        assert_eq!(calls(&f), ["open -a Visual Studio Code /work/app", "open -a Cursor /work/app"]);
        f.editors.open_in_editor("/work/app", EditorId::ClaudeCode).await.unwrap();
        assert_eq!(fs::read_dir(&f.editors.terminal.tmp_dir).unwrap().count(), 1);
        let missing = Editors { open_command: "/no/open".into(), ..f.editors.clone() };
        assert!(missing.open_in_editor("/w", EditorId::Cursor).await.is_err());
    }

    #[tokio::test]
    async fn a_claude_code_review_names_the_session_and_the_mode_and_a_resume_reopens_the_old_one() {
        let f = fx();
        let session = ReviewSession { new_id: Some("sess-1".into()), resume_id: None };
        let r = f.editors.open_for_review("/w", EditorId::ClaudeCode, "Review it", "plan", &[], &TerminalOptions::default(), &session).await.unwrap();
        assert_eq!((r.opened_editor, r.review_started, r.review_note.as_str()), (false, true, "Claude Code is reviewing it in a new terminal window."));
        let read = |n: usize| {
            let mut dirs: Vec<_> = fs::read_dir(&f.editors.terminal.tmp_dir).unwrap().map(|e| e.unwrap().path()).collect();
            dirs.sort();
            fs::read_to_string(dirs[n].join("review.sh")).unwrap()
        };
        assert!(read(0).contains("'claude' '--settings' '{\"ultracode\":true}' '--session-id' 'sess-1' '--permission-mode' 'plan' 'Review it'"), "{}", read(0));
        let resume = ReviewSession { new_id: None, resume_id: Some("old-1".into()) };
        let r = f.editors.open_for_review("/w", EditorId::ClaudeCode, "ignored", "auto", &[], &TerminalOptions::default(), &resume).await.unwrap();
        assert_eq!(r.review_note, "Reopened the last review's Claude Code session in a new terminal window.");
        let both = format!("{}{}", read(0), read(1));
        assert!(both.contains("'claude' '--settings' '{\"ultracode\":true}' '--resume' 'old-1' '--permission-mode' 'auto'") && !both.contains("ignored"), "{both}");
    }

    #[tokio::test]
    async fn cursor_opens_the_folder_then_sends_the_prompt_deeplink() {
        let f = fx();
        let r = f.editors.open_for_review("/w/app", EditorId::Cursor, "Review it", "auto", &[], &TerminalOptions::default(), &ReviewSession::default()).await.unwrap();
        assert!(r.opened_editor && r.review_started && r.review_note.contains("Click Create Chat"));
        let c = calls(&f);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], "open -a Cursor /w/app");
        assert!(c[1].starts_with("open cursor://"), "{c:?}");
        // A prompt Cursor would refuse is reported, with the folder still opened.
        let long = "x".repeat(20_000);
        let r = f.editors.open_for_review("/w/app", EditorId::Cursor, &long, "auto", &[], &TerminalOptions::default(), &ReviewSession::default()).await.unwrap();
        assert!(r.opened_editor && !r.review_started && r.review_note.starts_with("Couldn't start the automatic review: "), "{r:?}");
        assert_eq!(calls(&f).len(), 2, "nothing was opened for the refused prompt");
    }

    #[tokio::test]
    async fn vscode_opens_the_folder_then_runs_code_chat_in_it_and_a_missing_cli_is_reported_not_fatal() {
        let f = fx();
        let log = f.root.join("code.txt");
        let code = script(&f.root.join("bundled-code"), &format!("echo \"$PWD|$*\" > '{}'", log.display()));
        let r = f.editors.open_for_review(f.root.to_str().unwrap(), EditorId::Vscode, "Review it", "auto", &[], &TerminalOptions::default(), &ReviewSession::default()).await.unwrap();
        assert!(r.review_started && r.review_note == "Copilot Chat is reviewing it in Ask mode (read-only).");
        assert_eq!(fs::read_to_string(&log).unwrap().trim(), format!("{}|chat --mode ask Review it", f.root.display()));
        let _ = code;
        // No bundled CLI and no `code` on PATH: the review isn't started, but the folder opened.
        fs::remove_file(f.root.join("bundled-code")).unwrap();
        let r = f.editors.open_for_review(f.root.to_str().unwrap(), EditorId::Vscode, "x", "auto", &[], &TerminalOptions::default(), &ReviewSession::default()).await.unwrap();
        assert!(!r.review_started && r.opened_editor && r.review_note == "Couldn't start the automatic review: the `code` command isn't installed.", "{r:?}");
    }

    #[tokio::test]
    async fn editors_other_than_claude_code_are_macos_only_by_default() {
        if !cfg!(target_os = "macos") {
            let e = Editors::default();
            assert_eq!(e.open_in_editor("/w", EditorId::Vscode).await.unwrap_err(), "Opening Visual Studio Code is only implemented for macOS.");
            let r = e.open_for_review("/w", EditorId::Cursor, "p", "auto", &[], &TerminalOptions::default(), &ReviewSession::default()).await.unwrap();
            assert!(!r.review_started && r.review_note.contains("only implemented for macOS"));
        }
    }
}
