//! Launching a new terminal window/tab, cd'd into a folder, running `claude` or
//! another command (port of `core/terminal.ts`).
//!
//! There's no URL scheme for this (unlike editors) since "open a terminal and
//! run a command" isn't something the OS lets a browser link do directly, so it
//! goes through the companion service, which has OS access. macOS only
//! (osascript). Prefers iTerm if installed, falling back to Terminal.app.

use crate::claude_args::shell_quote_single as shell_quote;
use crate::ids::uuid_v4;
use crate::terminal_focus::{parse_terminal_location, TerminalLocation};
use base64::Engine;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderStyle {
    Title,
    Heading,
    Rule,
    Label,
    Muted,
    Branch,
    Target,
    Path,
    Link,
    Status,
}

impl HeaderStyle {
    /// SGR codes per style. The basic 16 colors only, so the header follows the user's terminal
    /// theme instead of fighting it.
    fn sgr(self) -> &'static str {
        match self {
            Self::Title => "1;36",
            Self::Heading => "1",
            Self::Rule => "36",
            Self::Label | Self::Muted => "90",
            Self::Branch => "1;32",
            Self::Target => "1;33",
            Self::Path => "1",
            Self::Link => "4;36",
            Self::Status => "1;35",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "title" => Self::Title,
            "heading" => Self::Heading,
            "rule" => Self::Rule,
            "label" => Self::Label,
            "muted" => Self::Muted,
            "branch" => Self::Branch,
            "target" => Self::Target,
            "path" => Self::Path,
            "link" => Self::Link,
            "status" => Self::Status,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderSegment {
    pub text: String,
    pub style: Option<HeaderStyle>,
}

impl HeaderSegment {
    pub fn plain(text: &str) -> Self {
        Self { text: text.into(), style: None }
    }
    pub fn styled(text: &str, style: HeaderStyle) -> Self {
        Self { text: text.into(), style: Some(style) }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TerminalOptions {
    /// Tab/window title.
    pub title: Option<String>,
    /// iTerm's badge: large text drawn behind the session's own, so it stays visible under a
    /// full-screen program that overwrites the title. Keep it one short line.
    pub badge: Option<String>,
    /// Extra environment variables for the command only.
    pub env: Vec<(String, String)>,
    /// A review-session status file for the script to record the shell, the command's process
    /// and its exit code in.
    pub status_file: Option<String>,
    /// Open in a new tab of the terminal's current window (a window when it has none) instead of
    /// a new window.
    pub new_tab: bool,
}

/// Run by `sh -c` with the status file as $0 and the command as "$@": the process records
/// itself, then runs the command as its child and exits with its status. Not exec: cancelling
/// signals only this process's descendants, so it exits normally and the rest of the sourced
/// script still runs (a foreground job killed by SIGINT would abort it).
const RECORD_AND_RUN: &str = r#"printf 'agent %s %s\n' "$$" "$(LC_ALL=C ps -o lstart= -p $$)" >> "$0" && "$@""#;

/// Starts the session with ultracode on (dynamic workflows on every task), as `/effort ultracode`
/// does in a session: the `ultracode` settings key, session-scoped, passed inline.
pub const ULTRACODE_ARGS: [&str; 2] = ["--settings", r#"{"ultracode":true}"#];

/// The arguments every interactive Claude Code session the companion starts begins with: ultracode
/// and, from an install tree, the bundled mod.
pub fn session_args() -> Vec<String> {
    let mut args: Vec<String> = ULTRACODE_ARGS.iter().map(|a| a.to_string()).collect();
    args.extend(bundled_mod_args());
    args
}

/// `--plugin-dir <install>/mods/run-pane` when the running binary sits in an install tree
/// that has the mod (`<install>/bin/<binary>`, `<install>/mods/run-pane`), so every
/// Claude Code session the companion starts has the Run pane with nothing to install. Nothing
/// otherwise (a dev build, tests).
pub fn bundled_mod_args() -> Vec<String> {
    let Ok(exe) = std::env::current_exe().and_then(|e| e.canonicalize()) else { return vec![] };
    exe.parent().and_then(Path::parent).map(bundled_mod_args_in).unwrap_or_default()
}

fn bundled_mod_args_in(install_dir: &Path) -> Vec<String> {
    let dir = install_dir.join("mods").join("run-pane");
    if dir.join(".claude-plugin").join("plugin.json").is_file() {
        vec!["--plugin-dir".into(), dir.to_string_lossy().into_owned()]
    } else {
        vec![]
    }
}

/// The tab title for a ticket's terminal: the key and a few words of the Jira title, cut at a word
/// (`PROJ-12 Fix login redirect loop`). Just the key when there is no title.
pub fn ticket_title(key: &str, summary: Option<&str>) -> String {
    const MAX: usize = 40;
    let words: Vec<&str> = summary.map(|s| s.split_whitespace().collect()).unwrap_or_default();
    let mut short = String::new();
    for w in &words {
        if !short.is_empty() && short.chars().count() + 1 + w.chars().count() > MAX {
            short.push('\u{2026}');
            break;
        }
        if !short.is_empty() {
            short.push(' ');
        }
        short.push_str(w);
    }
    if short.chars().count() > MAX + 1 {
        short = short.chars().take(MAX).collect::<String>() + "\u{2026}";
    }
    strip_controls(&if short.is_empty() { key.to_string() } else { format!("{key} {short}") })
}

/// Control and C1 characters, which can't go into a title or badge.
fn strip_controls(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}') { ' ' } else { c }).collect()
}

/// The shell command `run_in_terminal` runs: `header` is printed on a cleared screen, so it
/// replaces the typed command at the top of the window, then `argv` runs in `cwd`. The color codes
/// are only ever in printf's format string; segment text is passed as quoted arguments, so it's
/// printed verbatim and can't inject escape sequences or format directives. The title and badge
/// escapes follow the same rule; the badge is base64 as iTerm requires, with backslashes doubled
/// so iTerm's badge format doesn't interpolate `\(...)` in it.
pub fn build_terminal_command(cwd: &str, argv: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<String, String> {
    let mut parts = vec![format!("cd {}", shell_quote(cwd))];
    if let Some(title) = options.title.as_deref().filter(|t| !t.is_empty()) {
        parts.push(format!(r"printf '\033]0;%s\007' {}", shell_quote(&strip_controls(title))));
    }
    if let Some(badge) = options.badge.as_deref().filter(|b| !b.is_empty()) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(strip_controls(badge).replace('\\', "\\\\"));
        parts.push(format!(r"printf '\033]1337;SetBadgeFormat=%s\007' {}", shell_quote(&encoded)));
    }
    if !header.is_empty() {
        let mut format = String::new();
        let mut args: Vec<String> = vec![];
        for line in header {
            for seg in line {
                match seg.style {
                    Some(style) => format.push_str(&format!(r"\033[{}m%s\033[0m", style.sgr())),
                    None => format.push_str("%s"),
                }
                args.push(shell_quote(&seg.text));
            }
            format.push_str(r"\n");
        }
        parts.push("clear".into());
        parts.push(format!("printf '{format}' {}", args.join(" ")).trim_end().to_string());
    }
    let mut words: Vec<String> = Vec::new();
    for (name, value) in &options.env {
        let mut chars = name.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(format!("Invalid environment variable name: {name}"));
        }
        words.push(format!("{name}={}", shell_quote(value)));
    }
    words.extend(argv.iter().map(|a| shell_quote(a)));
    parts.push(words.join(" "));
    Ok(parts.join(" && "))
}

/// POSIX sh script (sourced, so `$0` isn't its path) that deletes itself and its private dir
/// before running `build_terminal_command`'s command.
pub fn build_terminal_script(script_path: &str, cwd: &str, argv: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<String, String> {
    let dir = Path::new(script_path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let cleanup = format!("rm -f {}; rmdir {}", shell_quote(script_path), shell_quote(&dir));
    let Some(status_file) = options.status_file.as_deref() else {
        return Ok(format!("{cleanup}\n{}\n", build_terminal_command(cwd, argv, header, options)?));
    };
    let status = shell_quote(status_file);
    // $$ in a sourced script is the user's shell. After a cancel, the killed full-screen agent may
    // have left the terminal in raw mode and on its alternate screen, so both are reset before
    // the prompt comes back.
    let mut tracked: Vec<String> = vec!["sh".into(), "-c".into(), RECORD_AND_RUN.into(), status_file.into()];
    tracked.extend(argv.iter().cloned());
    Ok([
        cleanup,
        format!(r#"printf 'shell %s %s\n' "$$" "$(LC_ALL=C ps -o lstart= -p $$)" >> {status}"#),
        build_terminal_command(cwd, &tracked, header, options)?,
        format!(r#"printf 'exit %s\n' "$?" >> {status}"#),
        format!(r"if grep -qx cancel {status}; then stty sane 2>/dev/null; printf '\033[?1049l\033[?25h\033[1;35m%s\033[0m\n' 'Review cancelled.'; fi"),
        String::new(),
    ]
    .join("\n"))
}

/// AppleScript string literal: escapes backslashes and double quotes so `text` can never break
/// out of the quotes it's placed in.
fn apple_script_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The osascript that opens a new window and types `command` into it. The title is also set here,
/// on the tab itself (iTerm's session name, Terminal's custom title), because cursor-agent
/// overwrites any title set by escape sequence. It's set after the command is typed and in a
/// `try`, so a terminal that refuses it still runs the review. It prints where the command runs,
/// for `terminal_focus` (empty if that can't be read).
pub fn build_terminal_apple_script(use_iterm: bool, command: &str, title: Option<&str>) -> String {
    build_terminal_apple_script_in(use_iterm, command, title, false)
}

/// `build_terminal_apple_script`, in a new tab of the current window when `in_tab` (a new window
/// when the terminal has none). iTerm makes the tab itself. Terminal.app can only make one with
/// Cmd-T through System Events, which needs the Accessibility permission, so without it the
/// command opens a window instead.
pub fn build_terminal_apple_script_in(use_iterm: bool, command: &str, title: Option<&str>, in_tab: bool) -> String {
    let cmd = apple_script_string(command);
    let name = title.filter(|t| !t.is_empty()).map(|t| apple_script_string(&strip_controls(t)));
    if use_iterm {
        let set_name = name.map(|n| format!("try\n            set name to {n}\n          end try")).unwrap_or_default();
        let make_target = if in_tab {
            "if (count of windows) is 0 then\n          set newWindow to (create window with default profile)\n        else\n          set newWindow to current window\n          tell newWindow to create tab with default profile\n        end if"
        } else {
            "set newWindow to (create window with default profile)"
        };
        format!(
            r#"
      tell application "iTerm"
        activate
        {make_target}
        tell current session of newWindow
          write text {cmd}
          {set_name}
        end tell
        set loc to ""
        try
          set loc to "iterm " & (id of newWindow) & " " & (id of current session of newWindow)
        end try
        return loc
      end tell
    "#
        )
    } else {
        let make_tab = if in_tab {
            format!("if (count of windows) is 0 then\n          set newTab to do script {cmd}\n        else\n          try\n            tell application \"System Events\" to keystroke \"t\" using command down\n            delay 0.4\n            set newTab to do script {cmd} in front window\n          on error\n            set newTab to do script {cmd}\n          end try\n        end if")
        } else {
            format!("set newTab to do script {cmd}")
        };
        let set_title = name.map(|n| format!("try\n          set custom title of newTab to {n}\n          set title displays custom title of newTab to true\n        end try")).unwrap_or_default();
        format!(
            r#"
      tell application "Terminal"
        activate
        {make_tab}
        {set_title}
        set loc to ""
        try
          set loc to "terminal " & (id of front window) & " " & (tty of newTab)
        end try
        return loc
      end tell
    "#
        )
    }
}

/// Writes the script to a fresh 0700 temp dir; returns the script path.
pub fn write_terminal_script(tmp: &Path, cwd: &str, argv: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<PathBuf, String> {
    let dir = tmp.join(format!("pr-review-{}", &uuid_v4()[..8]));
    fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("review.sh");
    let script = build_terminal_script(&path.to_string_lossy(), cwd, argv, header, options)?;
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| e.to_string())?;
    f.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

/// What runs the terminal: the tools and places it touches, injectable for tests.
#[derive(Debug, Clone)]
pub struct Terminal {
    pub osascript: String,
    pub iterm_app: PathBuf,
    pub tmp_dir: PathBuf,
    pub require_macos: bool,
}

impl Default for Terminal {
    fn default() -> Self {
        Self { osascript: "osascript".into(), iterm_app: "/Applications/iTerm.app".into(), tmp_dir: std::env::temp_dir(), require_macos: true }
    }
}

impl Terminal {
    /// Launches a new terminal window/tab, cd'd into `cwd`, running `claude` with `args`. Claude
    /// Code otherwise retitles the tab with its own task summary; with that off the title stays
    /// put, so no badge is needed.
    pub async fn open_claude_code(&self, cwd: &str, args: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<Option<TerminalLocation>, String> {
        if self.require_macos && !cfg!(target_os = "macos") {
            return Err("\"Open in Claude Code\" is only implemented for macOS.".into());
        }
        let has_title = options.title.as_deref().is_some_and(|t| !t.is_empty());
        let mut options = options.clone();
        if has_title {
            options.env.push(("CLAUDE_CODE_DISABLE_TERMINAL_TITLE".into(), "1".into()));
            options.badge = None;
        }
        let mut argv = vec!["claude".to_string()];
        argv.extend(session_args());
        argv.extend(args.iter().cloned());
        self.run_in_terminal(cwd, &argv, header, &options).await
    }

    /// A new tab of the terminal's current window (a window when it has none) with a plain interactive shell in `cwd`. The window's own shell
    /// sources the script, so after `true` returns it stays in `cwd` at its prompt.
    pub async fn open_shell(&self, cwd: &str, title: &str) -> Result<Option<TerminalLocation>, String> {
        let header = vec![vec![HeaderSegment::styled("Shell", HeaderStyle::Title), HeaderSegment::plain("  "), HeaderSegment::styled(cwd, HeaderStyle::Path)]];
        self.run_in_terminal(cwd, &["true".to_string()], &header, &TerminalOptions { title: Some(title.into()), new_tab: true, ..Default::default() }).await
    }

    /// The tab title for `open_shell`: the folder's name and `shell`.
    pub fn shell_title(cwd: &str) -> String {
        let name = Path::new(cwd.trim_end_matches('/')).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        strip_controls(&if name.is_empty() { "shell".to_string() } else { format!("{name} \u{b7} shell") })
    }

    /// Same as `open_claude_code`, but for any argv. Resolves where it opened, if the terminal said.
    pub async fn run_in_terminal(&self, cwd: &str, argv: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<Option<TerminalLocation>, String> {
        if self.require_macos && !cfg!(target_os = "macos") {
            return Err("Opening a terminal is only implemented for macOS.".into());
        }
        let use_iterm = self.iterm_app.exists();
        // Typed input is truncated at the tty's ~1024-byte line limit (MAX_CANON), and a review
        // prompt plus header is longer than that, so only a short line is typed. Sourced, not
        // executed, so the agent runs in the user's interactive shell (their env/PATH) and it
        // stays in `cwd` afterwards.
        let options = if use_iterm { options.clone() } else { TerminalOptions { badge: None, ..options.clone() } };
        let script_path = write_terminal_script(&self.tmp_dir, cwd, argv, header, &options)?;
        let script = build_terminal_apple_script_in(use_iterm, &format!(". {}", shell_quote(&script_path.to_string_lossy())), options.title.as_deref(), options.new_tab);
        let remove = || {
            if let Some(dir) = script_path.parent() {
                let _ = fs::remove_dir_all(dir);
            }
        };
        let out = match Command::new(&self.osascript).args(["-e", &script]).output().await {
            Ok(o) => o,
            Err(e) => {
                remove();
                return Err(e.to_string());
            }
        };
        if !out.status.success() {
            remove();
            return Err(format!("osascript exited with {}: {}", out.status.code().map_or("null".to_string(), |c| c.to_string()), String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(parse_terminal_location(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Runs a built command line through a real `sh` and returns its stdout.
    fn sh(cmd: &str) -> String {
        let out = std::process::Command::new("sh").args(["-c", cmd]).output().unwrap();
        assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn shell_words_are_quoted_so_text_cannot_inject_commands() {
        let evil = "it's $(touch /tmp/pwned-by-terminal-test); `id` \"x\"";
        let cmd = build_terminal_command("/tmp", &sv(&["printf", "%s", evil]), &[], &TerminalOptions::default()).unwrap();
        assert_eq!(sh(&cmd), evil);
        assert!(!Path::new("/tmp/pwned-by-terminal-test").exists());
        assert!(cmd.starts_with("cd '/tmp' && 'printf' '%s' "));
    }

    #[test]
    fn the_header_is_printed_verbatim_with_styles_only_in_the_format_and_the_command_then_runs_in_cwd() {
        let header = vec![vec![HeaderSegment::styled("Review %s \\033[31m", HeaderStyle::Title), HeaderSegment::plain(" PR #4")], vec![HeaderSegment::styled("x", HeaderStyle::Rule)]];
        let cmd = build_terminal_command("/", &sv(&["pwd"]), &header, &TerminalOptions::default()).unwrap();
        assert_eq!(cmd, r"cd '/' && clear && printf '\033[1;36m%s\033[0m%s\n\033[36m%s\033[0m\n' 'Review %s \033[31m' ' PR #4' 'x' && 'pwd'");
        let out = sh(&cmd.replace("clear && ", ""));
        assert_eq!(out, "\u{1b}[1;36mReview %s \\033[31m\u{1b}[0m PR #4\n\u{1b}[36mx\u{1b}[0m\n/\n");
    }

    #[test]
    fn a_session_starts_with_ultracode_on_and_then_the_bundled_mod_when_installed() {
        let args = session_args();
        assert_eq!(&args[..2], ["--settings", r#"{"ultracode":true}"#]);
        // A test binary is in no install tree, so nothing follows.
        assert_eq!(args.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(parsed["ultracode"], true);
    }

    #[test]
    fn the_bundled_mod_is_loaded_only_from_an_install_tree_that_has_it() {
        let d = tempfile::tempdir().unwrap();
        assert!(bundled_mod_args_in(d.path()).is_empty());
        let manifest = d.path().join("mods/run-pane/.claude-plugin/plugin.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, "{}").unwrap();
        let args = bundled_mod_args_in(d.path());
        assert_eq!(args[0], "--plugin-dir");
        assert!(args[1].ends_with("mods/run-pane"));
        // A test binary is in no install tree.
        assert!(bundled_mod_args().is_empty());
    }

    #[test]
    fn a_tab_is_made_in_the_current_window_and_a_window_only_when_there_is_none() {
        let iterm = build_terminal_apple_script_in(true, ". '/tmp/x'", Some("t"), true);
        assert!(iterm.contains("create tab with default profile") && iterm.contains("set newWindow to current window"));
        assert!(iterm.contains("if (count of windows) is 0 then"));
        let term = build_terminal_apple_script_in(false, ". '/tmp/x'", None, true);
        assert!(term.contains("keystroke \"t\" using command down") && term.contains("do script \". '/tmp/x'\" in front window"));
        assert!(term.contains("on error"));
        // Not in a tab: unchanged, a window.
        let win = build_terminal_apple_script_in(true, "c", None, false);
        assert!(win.contains("set newWindow to (create window with default profile)") && !win.contains("create tab"));
        assert_eq!(win, build_terminal_apple_script(true, "c", None));
    }

    #[test]
    fn a_shell_title_is_the_folder_name_and_shell() {
        assert_eq!(Terminal::shell_title("/w/sample-app/"), "sample-app \u{b7} shell");
        assert_eq!(Terminal::shell_title("/"), "shell");
        assert_eq!(Terminal::shell_title("/w/a\nb"), "a b \u{b7} shell");
    }

    #[test]
    fn a_ticket_title_is_the_key_and_a_few_words_cut_at_a_word() {
        assert_eq!(ticket_title("PROJ-1", Some("  Fix   the login\n redirect ")), "PROJ-1 Fix the login redirect");
        assert_eq!(ticket_title("PROJ-1", None), "PROJ-1");
        assert_eq!(ticket_title("PROJ-1", Some("  ")), "PROJ-1");
        let long = ticket_title("PROJ-1", Some("Activity colour is not displayed in the tooltip when hovering over a chart"));
        assert_eq!(long, "PROJ-1 Activity colour is not displayed in the\u{2026}");
        assert!(ticket_title("P-1", Some(&"x".repeat(200))).chars().count() <= 3 + 1 + 40 + 1);
        assert_eq!(ticket_title("P-1", Some("a\u{7}b\u{1b}]0;x")), "P-1 a b ]0;x");
    }

    #[test]
    fn the_title_and_badge_strip_controls_and_the_badge_is_base64_with_doubled_backslashes() {
        let o = TerminalOptions { title: Some("PR\u{7}\n4".into()), badge: Some("a\\(b)\u{1b}".into()), ..Default::default() };
        let cmd = build_terminal_command("/", &sv(&["true"]), &[], &o).unwrap();
        assert!(cmd.contains(r"printf '\033]0;%s\007' 'PR  4'"), "{cmd}");
        let expected = base64::engine::general_purpose::STANDARD.encode("a\\\\(b) ");
        assert!(cmd.contains(&format!(r"printf '\033]1337;SetBadgeFormat=%s\007' '{expected}'")), "{cmd}");
    }

    #[test]
    fn env_assignments_precede_the_command_and_bad_names_are_refused() {
        let o = TerminalOptions { env: vec![("A_1".into(), "x y".into())], ..Default::default() };
        assert!(build_terminal_command("/", &sv(&["env"]), &[], &o).unwrap().ends_with("&& A_1='x y' 'env'"));
        for bad in ["1A", "A-B", "", "A B", "A=B"] {
            let o = TerminalOptions { env: vec![(bad.into(), "v".into())], ..Default::default() };
            assert_eq!(build_terminal_command("/", &sv(&["env"]), &[], &o).unwrap_err(), format!("Invalid environment variable name: {bad}"));
        }
    }

    #[test]
    fn the_script_deletes_itself_and_with_a_status_file_records_the_shell_the_agent_and_the_exit_code() {
        let d = tempfile::tempdir().unwrap();
        let t = Terminal { tmp_dir: d.path().into(), ..Default::default() };
        let status = d.path().join("status.txt");
        let o = TerminalOptions { status_file: Some(status.to_string_lossy().into_owned()), ..Default::default() };
        let path = write_terminal_script(&t.tmp_dir, "/", &sv(&["sh", "-c", "exit 3"]), &[], &o).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        // Sourced by a shell, as the terminal does it.
        let out = std::process::Command::new("sh").args(["-c", &format!(". {}", shell_quote(&path.to_string_lossy()))]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!path.exists() && !path.parent().unwrap().exists(), "the script and its private dir are gone");
        let lines: Vec<String> = fs::read_to_string(&status).unwrap().lines().map(String::from).collect();
        assert!(lines[0].starts_with("shell ") && lines[1].starts_with("agent ") && lines[2] == "exit 3", "{lines:?}");
        // A cancel marker in the status file prints the notice.
        fs::write(&status, "cancel\n").unwrap();
        let again = write_terminal_script(&t.tmp_dir, "/", &sv(&["true"]), &[], &o).unwrap();
        let out = std::process::Command::new("sh").args(["-c", &format!(". {}", shell_quote(&again.to_string_lossy()))]).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("Review cancelled."));
    }

    #[test]
    fn without_a_status_file_the_script_is_just_cleanup_then_the_command() {
        let s = build_terminal_script("/tmp/pr-review-x/review.sh", "/w", &sv(&["claude"]), &[], &TerminalOptions::default()).unwrap();
        assert_eq!(s, "rm -f '/tmp/pr-review-x/review.sh'; rmdir '/tmp/pr-review-x'\ncd '/w' && 'claude'\n");
    }

    #[test]
    fn apple_script_strings_cannot_break_out_of_their_quotes_and_each_terminal_gets_its_own_script() {
        assert_eq!(apple_script_string(r#"a"b\c"#), r#""a\"b\\c""#);
        let iterm = build_terminal_apple_script(true, ". '/tmp/x'", Some("My \"PR\""));
        assert!(iterm.contains(r#"tell application "iTerm""#) && iterm.contains(r#"write text ". '/tmp/x'""#) && iterm.contains(r#"set name to "My \"PR\"""#));
        let term = build_terminal_apple_script(false, ". '/tmp/x'", None);
        assert!(term.contains(r#"tell application "Terminal""#) && term.contains("do script") && !term.contains("custom title"));
        assert!(build_terminal_apple_script(false, "c", Some("t")).contains("set custom title of newTab to \"t\""));
    }

    fn fake_osascript(dir: &Path, body: &str) -> String {
        let p = dir.join("osascript");
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn the_launcher_types_a_sourcing_line_and_reads_the_location_back_or_cleans_up_on_failure() {
        let d = tempfile::tempdir().unwrap();
        let tmp = d.path().join("tmp");
        fs::create_dir(&tmp).unwrap();
        let log = d.path().join("script.txt");
        let ok = Terminal { osascript: fake_osascript(d.path(), &format!("printf '%s' \"$2\" > '{}'; echo 'terminal 7 /dev/ttys003'", log.display())), iterm_app: "/no/iterm".into(), tmp_dir: tmp.clone(), require_macos: false };
        let loc = ok.open_claude_code("/w", &sv(&["--resume", "abc"]), &[], &TerminalOptions { title: Some("T".into()), badge: Some("B".into()), ..Default::default() }).await.unwrap();
        assert_eq!(loc, Some(TerminalLocation::Terminal { window_id: "7".into(), tty: "/dev/ttys003".into() }));
        let typed = fs::read_to_string(&log).unwrap();
        assert!(typed.contains("tell application \"Terminal\"") && typed.contains("do script \". '") && typed.contains("review.sh"), "{typed}");
        let leftover: Vec<_> = fs::read_dir(&tmp).unwrap().collect();
        assert_eq!(leftover.len(), 1, "the script stays until the terminal sources it");
        let script = fs::read_to_string(leftover[0].as_ref().unwrap().path().join("review.sh")).unwrap();
        assert!(script.contains("CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1' 'claude' '--settings' '{\"ultracode\":true}' '--resume' 'abc'") && !script.contains("1337"), "a title replaces the badge and stops Claude retitling: {script}");

        let bad = Terminal { osascript: fake_osascript(d.path(), "echo 'not allowed' >&2; exit 1"), tmp_dir: tmp.clone(), ..ok.clone() };
        let e = bad.run_in_terminal("/w", &sv(&["true"]), &[], &TerminalOptions::default()).await.unwrap_err();
        assert_eq!(e, "osascript exited with 1: not allowed");
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 1, "a failed launch removes its script");
        let missing = Terminal { osascript: "/no/osascript".into(), tmp_dir: tmp.clone(), ..ok };
        assert!(missing.run_in_terminal("/w", &sv(&["true"]), &[], &TerminalOptions::default()).await.is_err());
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn iterm_keeps_its_badge_and_it_is_macos_only_by_default() {
        let d = tempfile::tempdir().unwrap();
        let tmp = d.path().join("tmp");
        fs::create_dir(&tmp).unwrap();
        let t = Terminal { osascript: fake_osascript(d.path(), "echo ''"), iterm_app: d.path().to_path_buf(), tmp_dir: tmp.clone(), require_macos: false };
        assert_eq!(t.run_in_terminal("/w", &sv(&["true"]), &[], &TerminalOptions { badge: Some("B".into()), ..Default::default() }).await.unwrap(), None);
        let entry = fs::read_dir(&tmp).unwrap().next().unwrap().unwrap().path();
        assert!(fs::read_to_string(entry.join("review.sh")).unwrap().contains("SetBadgeFormat"));
        if !cfg!(target_os = "macos") {
            assert!(Terminal::default().open_claude_code("/w", &[], &[], &TerminalOptions::default()).await.unwrap_err().contains("only implemented for macOS"));
        }
    }
}
