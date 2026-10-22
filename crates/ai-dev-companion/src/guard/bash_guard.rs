//! PreToolUse hook for Claude's `--settings` (port of `core/bash-guard.js`),
//! wired with matcher `Bash` for policies with `bashGuard: true`.
//!
//! Invoked as `<binary> guard bash '<JSON array of allowed prefixes>'` with
//! the hook's JSON payload on stdin. Same wire protocol and fail-closed
//! contract as the edit guard: deny JSON + exit 0 for an ordinary deny, deny
//! JSON + exit 2 for anything unexpected (a bad argument), since only exit 2
//! blocks a call if the JSON itself isn't honored.

use super::GuardOutcome;
use super::bash_policy::is_bash_command_allowed_value;
use crate::js_text::js_trim;
use serde_json::Value;

const DENY_REASON: &str = "Bash is limited to git diff, git status and the configured check commands, run as one plain command \
(no ; & | ` $ ( ) < > or line breaks) \u{2014} blocked by the companion's bash guard";

fn deny(exit_code: i32) -> GuardOutcome {
    GuardOutcome::deny(DENY_REASON, exit_code)
}

/// `prefixes_arg` is argv[2] (None when absent); `stdin` is the hook payload.
pub fn run_bash_guard(prefixes_arg: Option<&str>, stdin: &str) -> GuardOutcome {
    // A missing or malformed prefix list is the CALLER's configuration error:
    // fail closed through exit 2.
    let prefixes: Value = match serde_json::from_str(prefixes_arg.unwrap_or("")) {
        Ok(v) => v,
        Err(_) => return deny(2),
    };
    let valid = prefixes
        .as_array()
        .is_some_and(|a| !a.is_empty() && a.iter().all(|p| p.as_str().is_some_and(|s| !js_trim(s).is_empty())));
    if !valid {
        return deny(2);
    }

    // Malformed stdin: no command to find below, so it denies (exit 0),
    // exactly like a call with no command.
    let payload: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let command = payload.get("tool_input").and_then(|i| i.get("command"));
    if is_bash_command_allowed_value(command, Some(&prefixes)) {
        GuardOutcome::allow()
    } else {
        deny(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PREFIXES: &str = r#"["git diff","git status","npm test"]"#;

    fn bash(command: &str) -> String {
        json!({ "tool_name": "Bash", "tool_input": { "command": command } }).to_string()
    }

    fn expected_deny() -> Value {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason":
                    "Bash is limited to git diff, git status and the configured check commands, run as one plain command \
                     (no ; & | ` $ ( ) < > or line breaks) \u{2014} blocked by the companion's bash guard",
            }
        })
    }

    #[test]
    fn allows_an_allowed_command_exit_0_no_output() {
        for cmd in ["git diff --stat", "git status", "npm test"] {
            let r = run_bash_guard(Some(PREFIXES), &bash(cmd));
            assert_eq!(r, GuardOutcome::allow(), "{cmd}");
        }
    }

    #[test]
    fn denies_a_command_outside_the_allowlist_or_chained_onto_it_exit_0_exact_deny_json() {
        for cmd in ["touch probe-made.txt && echo MADE", "git diff; rm x", "git status | sh", "git diffx", "rm -rf ."] {
            let r = run_bash_guard(Some(PREFIXES), &bash(cmd));
            assert_eq!(r.exit_code, 0, "{cmd}");
            assert_eq!(serde_json::from_str::<Value>(&r.stdout).unwrap(), expected_deny(), "{cmd}");
        }
    }

    #[test]
    fn denies_malformed_stdin_empty_stdin_and_a_missing_command() {
        let inputs = [
            "{not json".to_string(),
            String::new(),
            json!({ "tool_name": "Bash", "tool_input": {} }).to_string(),
            json!({ "tool_name": "Bash" }).to_string(),
            // Beyond the ported tests: an array tool_input, a null payload,
            // a non-string command.
            json!({ "tool_input": [] }).to_string(),
            "null".to_string(),
            json!({ "tool_input": { "command": 42 } }).to_string(),
        ];
        for input in inputs {
            let r = run_bash_guard(Some(PREFIXES), &input);
            assert_eq!(r.exit_code, 0, "{input}");
            assert_eq!(serde_json::from_str::<Value>(&r.stdout).unwrap(), expected_deny(), "{input}");
        }
    }

    #[test]
    fn fails_closed_deny_json_exit_2_on_a_missing_or_bad_prefixes_argument() {
        for arg in [None, Some(""), Some("not json"), Some("{}"), Some("[]"), Some(r#"["git diff", 3]"#), Some(r#"[""]"#)] {
            let r = run_bash_guard(arg, &bash("git diff"));
            assert_eq!(r.exit_code, 2, "{arg:?}");
            assert_eq!(serde_json::from_str::<Value>(&r.stdout).unwrap(), expected_deny(), "{arg:?}");
        }
    }
}
