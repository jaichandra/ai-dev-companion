//! Builds the URL that opens a file at a line in Cursor or VS Code (port of
//! `core/editor-url.js`). Both editors register a URL scheme on install
//! (`cursor://file/<path>:<line>:<col>` and `vscode://file/...`), so no
//! command-line tool has to be on PATH; the caller opens the URL with macOS
//! `open`. Pure.

use crate::js_number::{is_safe_integer, js_number};
use crate::uri::encode_uri_component;
use serde_json::Value;

const PREFERENCE: [&str; 2] = ["cursor", "vscode"];

fn scheme(editor: &str) -> Option<&'static str> {
    PREFERENCE.iter().copied().find(|s| *s == editor)
}

/// The editor to use: the configured one if it can open a file at a line and
/// is installed, else the first installed of Cursor, VS Code; `None` if
/// neither.
pub fn pick_editor<S: AsRef<str>>(configured: Option<&str>, installed: &[S]) -> Option<&'static str> {
    let has = |id: &str| installed.iter().any(|i| i.as_ref() == id);
    if let Some(s) = configured.and_then(scheme) {
        if has(s) {
            return Some(s);
        }
    }
    PREFERENCE.iter().copied().find(|id| has(id))
}

fn positive(value: Option<&Value>, fallback: u32, max: u32) -> u32 {
    let n = js_number(value);
    if is_safe_integer(n) && n >= 1.0 && n <= f64::from(max) { n as u32 } else { fallback }
}

/// `cursor://file/Users/me/a%20b/x.ts:10:5`, every path segment
/// percent-encoded. The line and column are untyped (text or numbers, as
/// they arrive in requests); out-of-range or missing values fall back to 1.
pub fn editor_url(editor: &str, abs_path: &str, line: Option<&Value>, column: Option<&Value>) -> Result<String, String> {
    let Some(scheme) = scheme(editor) else {
        return Err(format!("Can't open a file at a line in \"{editor}\"."));
    };
    if !abs_path.starts_with('/') || abs_path.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Err("A file location needs an absolute path.".into());
    }
    let encoded: Vec<String> = abs_path.split('/').map(encode_uri_component).collect();
    Ok(format!(
        "{scheme}://file{}:{}:{}",
        encoded.join("/"),
        positive(line, 1, 10_000_000),
        positive(column, 1, 100_000)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pick_editor_prefers_the_configured_editor_when_it_can_open_a_file_at_a_line_and_is_installed() {
        assert_eq!(pick_editor(Some("vscode"), &["claude-code", "cursor", "vscode"]), Some("vscode"));
        assert_eq!(pick_editor(Some("cursor"), &["cursor"]), Some("cursor"));
    }

    #[test]
    fn pick_editor_falls_back_to_cursor_then_vscode_and_never_picks_claude_code() {
        assert_eq!(pick_editor(Some("claude-code"), &["claude-code", "vscode", "cursor"]), Some("cursor"));
        assert_eq!(pick_editor(None, &["vscode"]), Some("vscode"));
        assert_eq!(pick_editor(Some("cursor"), &["vscode"]), Some("vscode"));
        assert_eq!(pick_editor(Some("vscode"), &["claude-code"]), None);
        assert_eq!(pick_editor(Some("__proto__"), &[] as &[&str]), None);
    }

    #[test]
    fn editor_url_builds_the_scheme_url_with_an_encoded_path_line_and_column() {
        let url = |e, p, l: Value, c: Value| editor_url(e, p, Some(&l), Some(&c)).unwrap();
        assert_eq!(url("cursor", "/Users/me/proj/src/a.ts", json!(10), json!(5)), "cursor://file/Users/me/proj/src/a.ts:10:5");
        assert_eq!(url("vscode", "/Users/me/my proj/a#b.ts", json!(3), json!(null)), "vscode://file/Users/me/my%20proj/a%23b.ts:3:1");
    }

    #[test]
    fn editor_url_defaults_and_clamps_the_position_and_refuses_bad_input() {
        let ok = |l: Value, c: Value| editor_url("cursor", "/a/b.ts", Some(&l), Some(&c)).unwrap();
        assert_eq!(ok(json!(0), json!(-4)), "cursor://file/a/b.ts:1:1");
        assert_eq!(ok(json!("12"), json!("3")), "cursor://file/a/b.ts:12:3");
        assert_eq!(editor_url("cursor", "/a/b.ts", None, None).unwrap(), "cursor://file/a/b.ts:1:1");
        assert_eq!(ok(json!(10_000_001), json!(100_001)), "cursor://file/a/b.ts:1:1");
        assert_eq!(ok(json!(10_000_000), json!(100_000)), "cursor://file/a/b.ts:10000000:100000");
        assert_eq!(ok(json!(1.5), json!("abc")), "cursor://file/a/b.ts:1:1");
        let err = |e, p| editor_url(e, p, Some(&json!(1)), Some(&json!(1))).unwrap_err();
        assert!(err("claude-code", "/a/b.ts").contains("Can't open a file at a line"));
        assert!(err("cursor", "relative/b.ts").contains("absolute path"));
        assert!(err("cursor", "/a/b\n.ts").contains("absolute path"));
        assert!(err("cursor", "/a/b\u{7f}.ts").contains("absolute path"));
    }
}
