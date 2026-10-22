//! PreToolUse hook for the Edit/Write/MultiEdit/NotebookEdit tools (port of
//! `core/edit-guard.js`), wired in for `worktreeWrite` runs. Fail-closed
//! confinement of those tools to one allowed root: the counterpart to the
//! Bash sandbox, which only ever mediates Bash (live-verified that Claude's
//! own Write tool still wrote outside a sandboxed cwd with no denial).
//!
//! Invoked as `<binary> guard edit '<allowed root>'`, hook JSON on stdin. The
//! decision is `path_policy::is_edit_allowed`.

use super::path_policy::is_edit_allowed;
use super::{GuardOutcome, tool_label, tool_name};
use serde_json::Value;

fn deny(tool: &str, exit_code: i32) -> GuardOutcome {
    GuardOutcome::deny(
        &format!("{} outside the worktree (or touching .git) is blocked by the companion's edit guard", tool_label(tool)),
        exit_code,
    )
}

/// `root_arg` is argv[2] (None when absent); `stdin` is the hook payload.
pub fn run_edit_guard(root_arg: Option<&str>, stdin: &str) -> GuardOutcome {
    // A missing or empty root is a configuration error on the CALLER's side:
    // fail closed through the deny-and-exit-2 path.
    let root = match root_arg {
        Some(r) if !r.is_empty() => r,
        _ => return deny("", 2),
    };
    // Malformed stdin: payload stays null, so the candidate lookup finds
    // nothing and denies, exactly like an unrecognized tool call.
    let payload: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let tool = tool_name(&payload);
    // Write/Edit/MultiEdit use `file_path`; NotebookEdit uses `notebook_path`.
    let input = payload.get("tool_input");
    let field = |name: &str| input.and_then(|i| i.get(name)).and_then(Value::as_str);
    match field("file_path").or_else(|| field("notebook_path")) {
        Some(candidate) if is_edit_allowed(root, candidate) => GuardOutcome::allow(),
        _ => deny(tool, 0),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn make_root() -> (tempfile::TempDir, String) {
        let dir = tempfile::Builder::new().prefix("edit-guard-test-").tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().to_str().unwrap().to_string();
        (dir, root)
    }

    fn run(root: &str, input: &Value) -> GuardOutcome {
        run_edit_guard(Some(root), &input.to_string())
    }

    fn reason(tool: &str) -> String {
        format!("{tool} outside the worktree (or touching .git) is blocked by the companion's edit guard")
    }

    fn decision(o: &GuardOutcome) -> Value {
        serde_json::from_str::<Value>(&o.stdout).unwrap()["hookSpecificOutput"]["permissionDecision"].clone()
    }

    #[test]
    fn allows_an_edit_inside_root_exit_0_no_output() {
        let (_d, root) = make_root();
        let file = format!("{root}/a.txt");
        fs::write(&file, "hi").unwrap();
        let r = run(&root, &json!({ "tool_name": "Edit", "tool_input": { "file_path": file } }));
        assert_eq!(r, GuardOutcome::allow());
    }

    #[test]
    fn allows_a_write_to_a_brand_new_file_inside_root() {
        let (_d, root) = make_root();
        let r = run(&root, &json!({ "tool_name": "Write", "tool_input": { "file_path": format!("{root}/new.txt") } }));
        assert_eq!(r, GuardOutcome::allow());
    }

    #[test]
    fn denies_a_write_outside_root_exit_0_exact_deny_json() {
        let (_d, root) = make_root();
        let outside = std::env::temp_dir().join("edit-guard-test-outside.txt");
        let r = run(&root, &json!({ "tool_name": "Write", "tool_input": { "file_path": outside } }));
        assert_eq!(r.exit_code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&r.stdout).unwrap(),
            json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason("Write"),
            } })
        );
    }

    #[test]
    fn denies_an_edit_touching_a_dot_git_path_inside_root() {
        let (_d, root) = make_root();
        fs::create_dir_all(format!("{root}/.git/hooks")).unwrap();
        let target = format!("{root}/.git/hooks/pre-commit");
        let r = run(&root, &json!({ "tool_name": "Edit", "tool_input": { "file_path": target } }));
        assert_eq!((r.exit_code, decision(&r)), (0, json!("deny")));
    }

    #[test]
    fn resolves_notebook_edits_notebook_path_field() {
        let (_d, root) = make_root();
        let outside = std::env::temp_dir().join("edit-guard-test-outside.ipynb");
        let r = run(&root, &json!({ "tool_name": "NotebookEdit", "tool_input": { "notebook_path": outside } }));
        assert_eq!((r.exit_code, decision(&r)), (0, json!("deny")));
        let inside = format!("{root}/n.ipynb");
        let ok = run(&root, &json!({ "tool_name": "NotebookEdit", "tool_input": { "notebook_path": inside } }));
        assert_eq!(ok, GuardOutcome::allow());
    }

    #[test]
    fn denies_malformed_and_empty_stdin_with_an_unknown_tool_reason_exit_0() {
        let (_d, root) = make_root();
        for input in ["not json", ""] {
            let r = run_edit_guard(Some(&root), input);
            assert_eq!(r.exit_code, 0, "{input:?}");
            assert_eq!(
                serde_json::from_str::<Value>(&r.stdout).unwrap()["hookSpecificOutput"]["permissionDecisionReason"],
                json!(reason("(unknown tool)")),
                "{input:?}"
            );
        }
    }

    #[test]
    fn fails_closed_exit_2_when_no_allowed_root_is_given() {
        let payload = json!({ "tool_name": "Edit", "tool_input": { "file_path": "/tmp/x" } }).to_string();
        for root in [None, Some("")] {
            let r = run_edit_guard(root, &payload);
            assert_eq!((r.exit_code, decision(&r)), (2, json!("deny")), "{root:?}");
        }
    }

    #[test]
    fn denies_a_tool_input_with_no_file_path_or_notebook_path() {
        let (_d, root) = make_root();
        for input in [
            json!({ "tool_name": "Edit", "tool_input": {} }),
            json!({ "tool_name": "Edit" }),
            json!({ "tool_name": "Edit", "tool_input": [] }),
            json!({ "tool_name": "Edit", "tool_input": { "file_path": 42 } }),
            json!(null),
        ] {
            let r = run(&root, &input);
            assert_eq!((r.exit_code, decision(&r)), (0, json!("deny")), "{input}");
        }
    }

    #[test]
    fn denies_a_write_through_a_dangling_symlink_that_points_outside_root() {
        let (_d, root) = make_root();
        let link = format!("{root}/link");
        symlink("../edit-guard-outside-new.txt", &link).unwrap();
        let r = run(&root, &json!({ "tool_name": "Write", "tool_input": { "file_path": link } }));
        assert_eq!((r.exit_code, decision(&r)), (0, json!("deny")));
    }
}
