//! Which terminal window a review was opened in, and bringing it back to the
//! front (port of `core/terminal-focus.js`).
//!
//! The launch AppleScript prints one line naming the new terminal (`iterm
//! <window id> <session id>` or `terminal <window id> <tty>`);
//! [`parse_terminal_location`] turns that into a location, and
//! [`focus_terminal`] selects it again. Locations are read back from disk, so
//! every field is checked against its expected shape on the way in and passed
//! to osascript as an argument, never pasted into the script.

use crate::exec::{ExecError, RunOptions, run};
use crate::js_text::{js_to_string, js_trim};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLocation {
    Iterm { window_id: String, session_id: String },
    Terminal { window_id: String, tty: String },
}

impl TerminalLocation {
    /// The JSON shape persisted with a job.
    pub fn to_json(&self) -> Value {
        match self {
            TerminalLocation::Iterm { window_id, session_id } => json!({ "app": "iterm", "windowId": window_id, "sessionId": session_id }),
            TerminalLocation::Terminal { window_id, tty } => json!({ "app": "terminal", "windowId": window_id, "tty": tty }),
        }
    }
}

/// `^\d{1,10}$`
fn is_window_id(s: &str) -> bool {
    (1..=10).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// iTerm's session ids are UUIDs: `^[0-9A-Fa-f]{8}(-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}$`.
fn is_iterm_session_id(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5 && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `^/dev/ttys\d{1,4}$`
fn is_tty(s: &str) -> bool {
    s.strip_prefix("/dev/ttys").is_some_and(|d| (1..=4).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()))
}

fn checked(app: &str, window_id: &str, id: &str) -> Option<TerminalLocation> {
    if !is_window_id(window_id) {
        return None;
    }
    match app {
        "iterm" if is_iterm_session_id(id) => Some(TerminalLocation::Iterm { window_id: window_id.into(), session_id: id.into() }),
        "terminal" if is_tty(id) => Some(TerminalLocation::Terminal { window_id: window_id.into(), tty: id.into() }),
        _ => None,
    }
}

/// A location read back from disk (untyped JSON), or `None` if any field has
/// the wrong shape. A numeric `windowId` is accepted, as `String(2)` is "2".
pub fn terminal_location_from_value(v: &Value) -> Option<TerminalLocation> {
    let o = v.as_object()?;
    let window_id = js_to_string(o.get("windowId")?);
    let text = |k: &str| o.get(k).and_then(Value::as_str);
    match o.get("app").and_then(Value::as_str)? {
        "iterm" => checked("iterm", &window_id, text("sessionId")?),
        "terminal" => checked("terminal", &window_id, text("tty")?),
        _ => None,
    }
}

pub fn is_valid_terminal_location(v: &Value) -> bool {
    terminal_location_from_value(v).is_some()
}

/// The location in the launch AppleScript's output, or `None`.
pub fn parse_terminal_location(text: &str) -> Option<TerminalLocation> {
    let parts: Vec<&str> = js_trim(text).split(' ').collect();
    let [app, window_id, id] = parts[..] else { return None };
    checked(app, window_id, id)
}

/// Neither launches its terminal if it isn't running (a closed review's
/// terminal can't be there), and both print "found" or "missing".
pub const FOCUS_SCRIPT_ITERM: &str = r#"
    on run argv
      set wanted to item 1 of argv
      if application "iTerm" is not running then return "missing"
      tell application "iTerm"
        repeat with w in windows
          repeat with t in tabs of w
            repeat with s in sessions of t
              if (id of s) is wanted then
                tell w to select
                tell t to select
                tell s to select
                activate
                return "found"
              end if
            end repeat
          end repeat
        end repeat
      end tell
      return "missing"
    end run
  "#;

pub const FOCUS_SCRIPT_TERMINAL: &str = r#"
    on run argv
      set wanted to item 1 of argv
      if application "Terminal" is not running then return "missing"
      tell application "Terminal"
        repeat with w in windows
          repeat with t in tabs of w
            if (tty of t) is wanted then
              set selected of t to true
              try
                set index of w to 1
              end try
              activate
              return "found"
            end if
          end repeat
        end repeat
      end tell
      return "missing"
    end run
  "#;

/// The osascript argv that focuses `loc`: the id goes in as an argument, never
/// inside the script.
pub fn focus_command(loc: &TerminalLocation) -> Vec<String> {
    match loc {
        TerminalLocation::Iterm { session_id, .. } => vec!["-e".into(), FOCUS_SCRIPT_ITERM.into(), session_id.clone()],
        TerminalLocation::Terminal { tty, .. } => vec!["-e".into(), FOCUS_SCRIPT_TERMINAL.into(), tty.clone()],
    }
}

/// Brings `loc`'s window/tab to the front; whether it was found.
pub async fn focus_terminal(loc: &TerminalLocation, osascript: &str) -> Result<bool, String> {
    let token = CancellationToken::new();
    let args = focus_command(loc);
    let opts = RunOptions { cancel: Some(token.clone()), ..Default::default() };
    let result = match tokio::time::timeout(Duration::from_secs(10), run(osascript, &args, opts)).await {
        Ok(r) => r,
        Err(_) => {
            token.cancel();
            return Err("osascript failed: timed out".into());
        }
    };
    match result {
        Ok(r) => Ok(r.stdout.trim() == "found"),
        Err(ExecError::Failed { result, .. }) => Err(format!("osascript failed: {}", js_trim(&result.stderr))),
        Err(e) => Err(format!("osascript failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0F3B5B28-6C1D-4E0A-9E2B-5A1C2D3E4F50";

    #[test]
    fn parses_itermss_window_and_session_ids_and_terminals_window_id_and_tty() {
        assert_eq!(
            parse_terminal_location(&format!("iterm 12345 {SESSION}\n")),
            Some(TerminalLocation::Iterm { window_id: "12345".into(), session_id: SESSION.into() })
        );
        assert_eq!(
            parse_terminal_location("terminal 678 /dev/ttys003\n"),
            Some(TerminalLocation::Terminal { window_id: "678".into(), tty: "/dev/ttys003".into() })
        );
    }

    #[test]
    fn parse_rejects_anything_else() {
        for text in [
            String::new(),
            "\n".into(),
            "iterm 1".into(),
            format!("iterm x {SESSION}"),
            "iterm 1 not-a-uuid".into(),
            format!("iterm 1 {SESSION}\" & do shell script \"id"),
            "terminal 1 /dev/ttys003 extra".into(),
            "terminal 1 /etc/passwd".into(),
            "terminal 1 /dev/ttys003\"".into(),
            "xterm 1 /dev/ttys003".into(),
            format!("iterm 12345678901 {SESSION}"),
            "terminal 1 /dev/ttys12345".into(),
            "terminal 1 /dev/ttys".into(),
            format!("iterm  1 {SESSION}"),
        ] {
            assert_eq!(parse_terminal_location(&text), None, "{text:?}");
        }
    }

    #[test]
    fn validation_checks_every_fields_shape() {
        let v = |x: Value| is_valid_terminal_location(&x);
        assert!(v(json!({ "app": "iterm", "windowId": "1", "sessionId": SESSION })));
        assert!(v(json!({ "app": "terminal", "windowId": 2, "tty": "/dev/ttys12" })));
        assert!(!v(json!({ "app": "terminal", "windowId": "2", "tty": "/dev/ttys12;rm" })));
        assert!(!v(json!({ "app": "iterm", "windowId": "1", "sessionId": 5 })));
        assert!(!v(json!({ "app": "iterm", "sessionId": SESSION })));
        assert!(!v(json!(null)));
        assert!(!v(json!("iterm 1 x")));
        let loc = terminal_location_from_value(&json!({ "app": "terminal", "windowId": 2, "tty": "/dev/ttys12" })).unwrap();
        assert_eq!(loc.to_json(), json!({ "app": "terminal", "windowId": "2", "tty": "/dev/ttys12" }));
    }

    #[test]
    fn focus_command_passes_the_id_as_an_argument_never_inside_the_script() {
        let iterm = TerminalLocation::Iterm { window_id: "1".into(), session_id: SESSION.into() };
        assert_eq!(focus_command(&iterm), ["-e", FOCUS_SCRIPT_ITERM, SESSION]);
        let term = TerminalLocation::Terminal { window_id: "1".into(), tty: "/dev/ttys003".into() };
        assert_eq!(focus_command(&term), ["-e", FOCUS_SCRIPT_TERMINAL, "/dev/ttys003"]);
        assert!(!FOCUS_SCRIPT_ITERM.contains(SESSION));
    }

    #[test]
    fn the_focus_scripts_never_start_a_terminal_that_isnt_running() {
        assert!(FOCUS_SCRIPT_ITERM.contains("if application \"iTerm\" is not running then return \"missing\""));
        assert!(FOCUS_SCRIPT_TERMINAL.contains("if application \"Terminal\" is not running then return \"missing\""));
    }

    #[tokio::test]
    async fn focus_terminal_reports_found_missing_and_failures_using_a_stand_in_osascript() {
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            p.to_string_lossy().into_owned()
        };
        let loc = TerminalLocation::Terminal { window_id: "1".into(), tty: "/dev/ttys003".into() };
        // A script just written can be "text file busy" while another test forks with its write
        // descriptor still open; that shows as an empty failure, so those runs are retried.
        let focus = |loc: &TerminalLocation, script: String| {
            let loc = loc.clone();
            async move {
                let mut r = focus_terminal(&loc, &script).await;
                for _ in 0..20 {
                    let busy = r.as_ref().err().is_some_and(|e| e == "osascript failed: " || e.to_lowercase().contains("busy"));
                    if !busy {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    r = focus_terminal(&loc, &script).await;
                }
                r
            }
        };
        assert_eq!(focus(&loc, fake("found", "echo found")).await, Ok(true));
        assert_eq!(focus(&loc, fake("missing", "echo missing")).await, Ok(false));
        assert_eq!(focus(&loc, fake("fails", "echo 'no permission' >&2; exit 1")).await, Err("osascript failed: no permission".into()));
        assert!(focus_terminal(&loc, "/no/such/osascript").await.unwrap_err().starts_with("osascript failed:"));
        // The id reaches the script as its third argument.
        assert_eq!(focus(&loc, fake("arg", "[ \"$3\" = /dev/ttys003 ] && echo found")).await, Ok(true));
    }
}
