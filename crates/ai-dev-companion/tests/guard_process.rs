//! The guards as real child processes, exactly how Claude's PreToolUse hook
//! invokes them: `<binary> guard <name> '<argument>'` with the hook JSON on
//! stdin. Mirrors the process-level Node tests (`*-guard.test.js`).

use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], stdin: &str) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ai-dev-companion"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap())
}

fn decision(stdout: &str) -> Value {
    serde_json::from_str::<Value>(stdout).unwrap()["hookSpecificOutput"]["permissionDecision"].clone()
}

#[test]
fn bash_guard_allows_denies_and_fails_closed() {
    let prefixes = r#"["git diff","git status","npm test"]"#;
    let bash = |c: &str| json!({ "tool_name": "Bash", "tool_input": { "command": c } }).to_string();
    assert_eq!(run(&["guard", "bash", prefixes], &bash("git diff --stat")), (0, String::new()));
    let (code, out) = run(&["guard", "bash", prefixes], &bash("git diff; rm x"));
    assert_eq!((code, decision(&out)), (0, json!("deny")));
    let (code, out) = run(&["guard", "bash"], &bash("git diff"));
    assert_eq!((code, decision(&out)), (2, json!("deny")));
}

#[test]
fn edit_and_read_guards_confine_to_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let root = root.to_str().unwrap();
    let file = format!("{root}/a.txt");
    std::fs::write(&file, "hi").unwrap();

    let edit = json!({ "tool_name": "Edit", "tool_input": { "file_path": file } }).to_string();
    assert_eq!(run(&["guard", "edit", root], &edit), (0, String::new()));
    let outside = json!({ "tool_name": "Write", "tool_input": { "file_path": "/etc/hosts" } }).to_string();
    let (code, out) = run(&["guard", "edit", root], &outside);
    assert_eq!((code, decision(&out)), (0, json!("deny")));

    let read = json!({ "tool_name": "Read", "tool_input": { "file_path": file } }).to_string();
    assert_eq!(run(&["guard", "read", root], &read), (0, String::new()));
    let escape = json!({ "tool_name": "Read", "tool_input": { "file_path": "/etc/hosts" } }).to_string();
    let (code, out) = run(&["guard", "read", root], &escape);
    assert_eq!((code, decision(&out)), (0, json!("deny")));
}

#[test]
fn an_unknown_guard_fails_closed_and_other_commands_are_not_handled() {
    let (code, out) = run(&["guard", "nope", "x"], "{}");
    assert_eq!((code, decision(&out)), (2, json!("deny")));
    let (code, _) = run(&["status"], "");
    assert_eq!(code, 64);
}
