//! PreToolUse hook for the Read/Glob/Grep tools (port of
//! `core/read-guard.js`): fail-closed confinement of those tools to the job's
//! worktree, wired in for both worktree policies with the same root as the
//! edit guard. Without it Claude's own Read tool could open any file the user
//! can, and the PR content it works from is untrusted input that may ask it to.
//!
//! Invoked as `<binary> guard read '<allowed root>'`, hook JSON on stdin. The
//! decision is `path_policy::is_read_tool_call_allowed`.

use super::path_policy::is_read_tool_call_allowed;
use super::{GuardOutcome, tool_label, tool_name};
use serde_json::Value;

fn deny(tool: &str, exit_code: i32) -> GuardOutcome {
    GuardOutcome::deny(
        &format!("{} outside the worktree is blocked by the companion's read guard", tool_label(tool)),
        exit_code,
    )
}

/// `root_arg` is argv[2] (None when absent); `stdin` is the hook payload.
pub fn run_read_guard(root_arg: Option<&str>, stdin: &str) -> GuardOutcome {
    let root = match root_arg {
        Some(r) if !r.is_empty() => r,
        _ => return deny("", 2),
    };
    // Malformed stdin: payload stays null and the decision below denies.
    let payload: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let tool = tool_name(&payload);
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    if is_read_tool_call_allowed(root, tool, input) { GuardOutcome::allow() } else { deny(tool, 0) }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn make_root() -> (tempfile::TempDir, String) {
        let dir = tempfile::Builder::new().prefix("read-guard-test-").tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().to_str().unwrap().to_string();
        (dir, root)
    }

    fn run(root: &str, input: &Value) -> GuardOutcome {
        run_read_guard(Some(root), &input.to_string())
    }

    fn deny_json(label: &str) -> Value {
        json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!("{label} outside the worktree is blocked by the companion's read guard"),
        } })
    }

    fn parsed(o: &GuardOutcome) -> Value {
        serde_json::from_str(&o.stdout).unwrap()
    }

    #[test]
    fn allows_a_read_inside_root_exit_0_no_output() {
        let (_d, root) = make_root();
        let file = format!("{root}/a.txt");
        fs::write(&file, "hi").unwrap();
        let r = run(&root, &json!({ "tool_name": "Read", "tool_input": { "file_path": file } }));
        assert_eq!(r, GuardOutcome::allow());
    }

    #[test]
    fn denies_a_read_outside_root_with_the_exact_deny_json() {
        let (_d, root) = make_root();
        let r = run(&root, &json!({ "tool_name": "Read", "tool_input": { "file_path": "/etc/hosts" } }));
        assert_eq!(r.exit_code, 0);
        assert_eq!(parsed(&r), deny_json("Read"));
    }

    #[test]
    fn allows_glob_and_grep_with_no_path_denies_one_with_a_path_outside_root() {
        let (_d, root) = make_root();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        for tool in ["Glob", "Grep"] {
            let ok = run(&root, &json!({ "tool_name": tool, "tool_input": { "pattern": "*.js" } }));
            assert_eq!(ok, GuardOutcome::allow(), "{tool}");
            let bad = run(&root, &json!({ "tool_name": tool, "tool_input": { "pattern": "*", "path": home } }));
            assert_eq!(bad.exit_code, 0, "{tool}");
            assert_eq!(parsed(&bad), deny_json(tool));
        }
    }

    #[test]
    fn denies_a_glob_whose_pattern_climbs_out_with_dot_dot() {
        let (_d, root) = make_root();
        let r = run(&root, &json!({ "tool_name": "Glob", "tool_input": { "pattern": "../../**/*" } }));
        assert_eq!(parsed(&r), deny_json("Glob"));
    }

    #[test]
    fn denies_malformed_stdin_with_an_unknown_tool_reason() {
        let (_d, root) = make_root();
        let r = run_read_guard(Some(&root), "not json");
        assert_eq!(r.exit_code, 0);
        assert_eq!(parsed(&r), deny_json("(unknown tool)"));
    }

    #[test]
    fn fails_closed_exit_2_with_no_root_argument() {
        let payload = json!({ "tool_name": "Read", "tool_input": { "file_path": "/tmp/x" } }).to_string();
        for root in [None, Some("")] {
            let r = run_read_guard(root, &payload);
            assert_eq!(r.exit_code, 2, "{root:?}");
            assert_eq!(parsed(&r)["hookSpecificOutput"]["permissionDecision"], json!("deny"));
        }
    }
}
