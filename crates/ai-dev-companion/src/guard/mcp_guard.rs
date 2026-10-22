//! PreToolUse hook for MCP tool calls (port of `core/mcp-guard.js`), wired in
//! only for policies with `mcpGuard: true` (the read-only ones), with matcher
//! `mcp__.*`. Invoked as `<binary> guard mcp` with the hook JSON (including
//! `tool_name`) on stdin.
//!
//! FAIL CLOSED: Claude Code treats ONLY hook exit code 2 as blocking a tool
//! call. An unexpected failure here would be as dangerous as returning
//! "allow", so there is no failure path that lets a call through: malformed or
//! empty stdin and a non-string `tool_name` all classify as the unknown tool
//! name, which is denied.

use super::mcp_classifier::{ToolClass, classify_mcp_tool};
use super::{GuardOutcome, tool_label};
use serde_json::Value;

pub fn run_mcp_guard(stdin: &str) -> GuardOutcome {
    let payload: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let tool = payload.get("tool_name").and_then(Value::as_str).filter(|t| !t.is_empty());
    match classify_mcp_tool(tool) {
        ToolClass::Allow => GuardOutcome::allow(),
        ToolClass::Deny => GuardOutcome::deny(
            &format!("{} is blocked by the companion's read-only policy", tool_label(tool.unwrap_or(""))),
            0,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reason(o: &GuardOutcome) -> String {
        serde_json::from_str::<Value>(&o.stdout).unwrap()["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn allows_a_read_only_mcp_tool_exit_0_no_output() {
        let r = run_mcp_guard(&json!({ "tool_name": "mcp__acme-jira-confluence__jira_get_issue" }).to_string());
        assert_eq!(r, GuardOutcome::allow());
    }

    #[test]
    fn denies_a_mutating_mcp_tool_exit_0_exact_deny_json() {
        let r = run_mcp_guard(&json!({ "tool_name": "mcp__acme-jenkins__stop_build" }).to_string());
        assert_eq!(r.exit_code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&r.stdout).unwrap(),
            json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "mcp__acme-jenkins__stop_build is blocked by the companion's read-only policy",
            } })
        );
    }

    #[test]
    fn denies_malformed_empty_and_non_string_tool_name_with_an_unknown_tool_reason() {
        for input in ["not json".to_string(), String::new(), json!({ "tool_name": 12345 }).to_string(), "null".into(), "{}".into()] {
            let r = run_mcp_guard(&input);
            assert_eq!(r.exit_code, 0, "{input:?}");
            assert_eq!(reason(&r), "(unknown tool) is blocked by the companion's read-only policy", "{input:?}");
        }
    }
}
