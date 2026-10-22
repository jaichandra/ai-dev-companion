//! The guard layer that restricts what Claude jobs may do. Pure decision
//! logic lives in the `*_policy` modules; the `*_guard` modules are the
//! PreToolUse hook entry points (same wire protocol as the Node scripts) and
//! `cli` dispatches `<binary> guard <name> ...` to them.
//!
//! Wire protocol, shared by every guard: the hook JSON arrives on stdin; a
//! deny is the `hookSpecificOutput` JSON on stdout. An ordinary deny exits 0;
//! anything unexpected (a bad argument) prints the same deny JSON and exits 2,
//! because only exit 2 blocks a call if the JSON itself isn't honored. Every
//! failure path denies.

pub mod bash_guard;
pub mod bash_policy;
pub mod cli;
pub mod edit_guard;
pub mod link_guard;
pub mod mcp_classifier;
pub mod mcp_guard;
pub mod path_policy;
pub mod read_guard;

use serde_json::{Value, json};

/// What the hook process should do: print `stdout`, exit with `exit_code`.
#[derive(Debug, PartialEq, Eq)]
pub struct GuardOutcome {
    pub stdout: String,
    pub exit_code: i32,
}

impl GuardOutcome {
    pub fn allow() -> Self {
        GuardOutcome { stdout: String::new(), exit_code: 0 }
    }

    /// The deny JSON with `reason`, exiting `exit_code` (0 ordinary, 2 error).
    pub fn deny(reason: &str, exit_code: i32) -> Self {
        let stdout = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string();
        GuardOutcome { stdout, exit_code }
    }
}

/// The tool name from a hook payload, or "" when absent or not a string.
pub(crate) fn tool_name(payload: &Value) -> &str {
    payload.get("tool_name").and_then(Value::as_str).unwrap_or("")
}

/// `toolName || "(unknown tool)"`: malformed stdin, a missing candidate path
/// and the hook's own internal-error path can all reach a deny with no
/// reliable tool name to report.
pub(crate) fn tool_label(name: &str) -> &str {
    if name.is_empty() { "(unknown tool)" } else { name }
}
