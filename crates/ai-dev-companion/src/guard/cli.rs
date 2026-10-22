//! `<binary> guard <name> <argument>`: how Claude's PreToolUse hooks reach the
//! guards. The Node build ran `<node> '<guard script>' '<argument>'`; now the
//! hook command is this binary with a `guard` subcommand, so nothing but the
//! binary is needed on the machine.

use super::GuardOutcome;
use super::bash_guard::run_bash_guard;
use super::edit_guard::run_edit_guard;
use super::mcp_guard::run_mcp_guard;
use super::read_guard::run_read_guard;
use std::io::{Read, Write};

/// The names the argument builder may put after `guard`.
pub const GUARD_NAMES: [&str; 4] = ["bash", "edit", "mcp", "read"];

fn read_stdin() -> String {
    let mut buf = Vec::new();
    // A read failure is treated as empty input, which every guard denies.
    let _ = std::io::stdin().read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Runs the named guard on `stdin` with its one argument. `None` for a name
/// that isn't a guard.
pub fn dispatch(name: &str, arg: Option<&str>, stdin: &str) -> Option<GuardOutcome> {
    match name {
        "bash" => Some(run_bash_guard(arg, stdin)),
        "edit" => Some(run_edit_guard(arg, stdin)),
        "mcp" => Some(run_mcp_guard(stdin)),
        "read" => Some(run_read_guard(arg, stdin)),
        _ => None,
    }
}

/// If `args` (the process arguments after the program name) start with
/// `guard`, runs that guard against the real stdin/stdout and returns the exit
/// code to use; otherwise `None` so the caller carries on with its own
/// subcommands. An unknown guard name fails closed like a bad argument: deny
/// JSON on stdout, exit 2.
pub fn run_if_guard(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("guard") {
        return None;
    }
    let name = args.get(1).map(String::as_str).unwrap_or("");
    let outcome = dispatch(name, args.get(2).map(String::as_str), &read_stdin())
        .unwrap_or_else(|| GuardOutcome::deny("unknown guard", 2));
    let mut out = std::io::stdout();
    let _ = out.write_all(outcome.stdout.as_bytes());
    let _ = out.flush();
    Some(outcome.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_by_name_and_rejects_unknown_names() {
        for name in GUARD_NAMES {
            assert!(dispatch(name, Some("/"), "{}").is_some(), "{name}");
        }
        assert!(dispatch("host", Some("/"), "{}").is_none());
        assert!(dispatch("", None, "").is_none());
    }

    #[test]
    fn only_the_guard_subcommand_is_handled() {
        assert_eq!(run_if_guard(&[]), None);
        assert_eq!(run_if_guard(&["status".to_string()]), None);
    }
}
