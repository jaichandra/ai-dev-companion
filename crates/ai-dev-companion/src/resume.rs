//! Pure logic for resuming a headless `claude` run in a terminal via
//! `claude --resume` (port of `core/resume.js`).

use crate::node_path;
use serde_json::Value;

const PERMISSION_MODES: [&str; 3] = ["plan", "auto", "default"];

/// Claude Code's own transcript-folder naming: the absolute cwd with every
/// non-alphanumeric character replaced by "-". Claude Code does this on JS
/// strings, so a character outside the BMP (two UTF-16 units) becomes "--".
pub fn claude_project_slug(cwd: &str) -> String {
    let mut out = String::with_capacity(cwd.len());
    for c in cwd.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    out
}

/// Where `claude --resume <id>` expects to find this session's transcript.
/// Used to tell a genuinely resumable session (its transcript still on disk)
/// from one that's gone.
pub fn session_transcript_path(home: &str, cwd: &str, id: &str) -> String {
    node_path::join(&[home, ".claude", "projects", &claude_project_slug(cwd), &format!("{id}.jsonl")])
}

/// `cwd`, resolved to its real path, for the transcript lookup ONLY: Claude
/// Code keys `~/.claude/projects/<slug>/` by the realpath'd cwd it was run in
/// (symlinks resolved, e.g. macOS's /tmp -> /private/tmp). The session's
/// STORED cwd stays exactly as captured. Falls back to the unresolved `cwd` if
/// realpath fails.
pub fn real_cwd_for_transcript(cwd: &str, realpath: impl FnOnce(&str) -> std::io::Result<String>) -> String {
    realpath(cwd).unwrap_or_else(|_| cwd.to_string())
}

/// `real_cwd_for_transcript` with the real filesystem.
pub fn real_cwd(cwd: &str) -> String {
    real_cwd_for_transcript(cwd, |p| std::fs::canonicalize(p).map(|c| c.to_string_lossy().into_owned()))
}

/// argv for `claude --resume`, kept in the same permission mode the original
/// headless run used.
pub fn build_resume_args(session_id: &str, permission_mode: &str) -> Vec<String> {
    ["--resume", session_id, "--permission-mode", permission_mode].map(String::from).to_vec()
}

/// `^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$`, case-insensitive.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Whether `session` (a job's `claudeSession`, or one read back from a JSON
/// file on disk) is safe to resume. This is the only gate between that value
/// and a shell command handed to a terminal, so every field is checked: `id`
/// must be a UUID (not arbitrary text passed to `claude --resume`), `cwd` an
/// absolute path, and `permissionMode` one of the modes a headless run ever
/// stamps onto a job.
pub fn validate_session(session: Option<&Value>) -> bool {
    let Some(s) = session else { return false };
    let text = |k: &str| s.get(k).and_then(Value::as_str);
    matches!(
        (text("id"), text("cwd"), text("permissionMode")),
        (Some(id), Some(cwd), Some(mode)) if is_uuid(id) && node_path::is_absolute(cwd) && PERMISSION_MODES.contains(&mode)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn claude_project_slug_replaces_every_non_alphanumeric_character_with_a_dash() {
        assert_eq!(
            claude_project_slug("/Users/jail/.bitbucket-ai-companion/worktrees/x"),
            "-Users-jail--bitbucket-ai-companion-worktrees-x"
        );
        assert_eq!(claude_project_slug("/a b/\u{e9}"), "-a-b--");
        assert_eq!(claude_project_slug("/\u{1F600}"), "---", "an astral character is two UTF-16 units");
    }

    #[test]
    fn session_transcript_path_joins_home_projects_the_slug_and_the_id() {
        assert_eq!(
            session_transcript_path("/Users/jail", "/Users/jail/work/repo", SESSION_ID),
            format!("/Users/jail/.claude/projects/-Users-jail-work-repo/{SESSION_ID}.jsonl")
        );
    }

    #[test]
    fn real_cwd_for_transcript_resolves_through_the_injected_realpath_or_falls_back() {
        let mut calls = vec![];
        let r = real_cwd_for_transcript("/Users/jail/work/repo", |p| {
            calls.push(p.to_string());
            Ok("/Users/jail/real/work/repo".into())
        });
        assert_eq!(r, "/Users/jail/real/work/repo");
        assert_eq!(calls, ["/Users/jail/work/repo"]);
        let r = real_cwd_for_transcript("/Users/jail/work/gone", |_| Err(std::io::Error::other("ENOENT")));
        assert_eq!(r, "/Users/jail/work/gone");
        assert_eq!(real_cwd("/definitely/not/here"), "/definitely/not/here");
    }

    #[test]
    fn build_resume_args_returns_the_flags_in_order() {
        assert_eq!(build_resume_args(SESSION_ID, "plan"), ["--resume", SESSION_ID, "--permission-mode", "plan"]);
    }

    #[test]
    fn validate_session_accepts_a_uuid_id_absolute_cwd_and_a_known_permission_mode() {
        for mode in ["plan", "auto", "default"] {
            assert!(validate_session(Some(&json!({ "id": SESSION_ID, "cwd": "/Users/jail/work", "permissionMode": mode }))), "{mode}");
        }
        assert!(validate_session(Some(&json!({ "id": SESSION_ID.to_uppercase(), "cwd": "/x", "permissionMode": "plan" }))));
    }

    #[test]
    fn validate_session_rejects_bad_ids_cwds_modes_and_missing_sessions() {
        let base = json!({ "id": SESSION_ID, "cwd": "/Users/jail/work", "permissionMode": "plan" });
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            b
        };
        for bad in [
            with("id", json!("not-a-uuid")),
            with("id", json!(null)),
            with("id", json!(format!("{SESSION_ID}\n"))),
            with("id", json!("1111111-22222-3333-4444-555555555555")),
            with("id", json!("gggggggg-2222-3333-4444-555555555555")),
            with("cwd", json!("relative/path")),
            with("cwd", json!(null)),
            with("permissionMode", json!("bypassPermissions")),
            with("permissionMode", json!(null)),
            json!({}),
            json!("x"),
        ] {
            assert!(!validate_session(Some(&bad)), "{bad}");
        }
        assert!(!validate_session(None));
        assert!(!validate_session(Some(&json!(null))));
    }
}
