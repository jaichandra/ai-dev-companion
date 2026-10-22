//! Finds "file:line[:column]" locations in a pasted stack trace or log excerpt
//! (JS/webpack, TypeScript, Java, Python, Go, Playwright/TestCafe); port of
//! `core/location-parse.js`. Pure. The text is untrusted (it comes from a web
//! page), so this only extracts candidates; the resolver decides whether one
//! names a real tracked file, and nothing here touches the filesystem.
//!
//! The Node pattern used a lookbehind (`(?<=^|[\s("'\[=>@,;])`) to require a
//! token boundary. This engine has none, so the boundary character is matched
//! (and skipped) instead. That is equivalent here: no boundary character can
//! be the last character of a match (a match ends in a digit or `)`), so a
//! consumed boundary never hides the start of a following match.
//!
//! `\s` is the JS whitespace set and `\w`/`\d`/`\b` are ASCII, as in JS.

use crate::js_text::{JS_WS_CLASS, js_trim, slice_utf16, utf16_len};
use crate::uri::decode_uri_component;
use regex::Regex;
use std::sync::LazyLock;

pub const MAX_TEXT: usize = 8192;
const MAX_RESULTS: usize = 20;
const MAX_PATH: usize = 500;

const SOURCE_EXT: &str = "(?:[cm]?[jt]sx?|vue|svelte|py|go|java|kt|kts|scala|groovy|rb|rs|cs|php|swift|cc|cpp|hpp|c|h|json|ya?ml|sh|css|scss)";

// Python:  File "/app/tests/test_login.py", line 57, in test_x
static PYTHON_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"File "([^"]+)", line ([0-9]+)"#).unwrap());

// Java/Kotlin/Scala/Groovy:  at com.example.ci.LoginPage.click(LoginPage.java:88)
static JAVA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?-u:\b)at[{ws}]+((?:[a-z_][A-Za-z0-9_$]*\.)+)[A-Z][A-Za-z0-9_$]*[A-Za-z0-9_$.<>]*\(([A-Za-z0-9_$]+\.(?:java|kt|scala|groovy)):([0-9]+)\)",
        ws = JS_WS_CLASS
    ))
    .unwrap()
});

// Everything else: path:line, path:line:col, path(line,col), with an optional
// webpack:// or file:// prefix and an optional ?query before the position. A
// path must start at a token boundary, so a URL such as
// https://cdn.example.com/bundle.js:1:2 (not a local file) yields nothing.
static GENERIC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?:^|[{ws}("'\[=>@,;])((?:(?:webpack|webpack-internal)://[^{ws}/]*/|file://)?([^{ws}:()'"<>|,;?]+?\.{ext})(?:(?:\?[^{ws}:()'"]*)?:([0-9]+)(?::([0-9]+))?|\(([0-9]+),([0-9]+)\)))"#,
        ws = JS_WS_CLASS,
        ext = SOURCE_EXT
    ))
    .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub column: Option<u32>,
}

fn clean_path(p: &str) -> Option<String> {
    let decoded = decode_uri_component(p).unwrap_or_else(|| p.to_string());
    let mut out = decoded.replace('\\', "/");
    while let Some(rest) = out.strip_prefix("./") {
        out = rest.to_string();
    }
    if out.is_empty() || utf16_len(&out) > MAX_PATH || out.split('/').any(|s| s == "..") {
        return None;
    }
    Some(out)
}

fn to_pos_int(s: &str) -> Option<u32> {
    let n: f64 = s.parse().ok()?;
    ((1.0..=10_000_000.0).contains(&n) && n.fract() == 0.0).then_some(n as u32)
}

struct Found {
    start: usize,
    end: usize,
    path: String,
    line: u32,
    column: Option<u32>,
}

/// Locations in the order found, de-duplicated, at most 20, from the first
/// 8 KB of `text`.
pub fn parse_locations(text: &str) -> Vec<Location> {
    if js_trim(text).is_empty() {
        return vec![];
    }
    let source = if utf16_len(text) > MAX_TEXT { slice_utf16(text, MAX_TEXT) } else { text.to_string() };
    let mut found: Vec<Found> = Vec::new();

    let add = |found: &mut Vec<Found>, start: usize, end: usize, raw_path: &str, raw_line: &str, raw_col: Option<&str>| {
        let (Some(path), Some(line)) = (clean_path(raw_path), to_pos_int(raw_line)) else { return };
        let column = raw_col.and_then(to_pos_int);
        found.push(Found { start, end, path, line, column });
    };

    for m in PYTHON_RE.captures_iter(&source) {
        let all = m.get(0).unwrap();
        add(&mut found, all.start(), all.end(), &m[1], &m[2], None);
    }
    for m in JAVA_RE.captures_iter(&source) {
        let all = m.get(0).unwrap();
        add(&mut found, all.start(), all.end(), &format!("{}{}", m[1].replace('.', "/"), &m[2]), &m[3], None);
    }
    let claimed: Vec<(usize, usize)> = found.iter().map(|f| (f.start, f.end)).collect();
    for m in GENERIC_RE.captures_iter(&source) {
        let all = m.get(1).unwrap(); // the token, without its boundary character
        let (start, end) = (all.start(), all.end());
        if claimed.iter().any(|&(s, e)| start < e && end > s) {
            continue; // already read as Python/Java
        }
        let line = m.get(3).or(m.get(5)).unwrap().as_str();
        let col = m.get(4).or(m.get(6)).map(|c| c.as_str());
        add(&mut found, start, end, &m[2], line, col);
    }

    found.sort_by_key(|f| f.start);
    let mut out: Vec<Location> = Vec::new();
    for f in found {
        let loc = Location { path: f.path, line: f.line, column: f.column };
        if out.contains(&loc) {
            continue;
        }
        out.push(loc);
        if out.len() >= MAX_RESULTS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Vec<String> {
        parse_locations(text)
            .iter()
            .map(|l| format!("{}:{}:{}", l.path, l.line, l.column.map_or("null".to_string(), |c| c.to_string())))
            .collect()
    }

    #[test]
    fn node_chrome_stack_frames_absolute_path_with_line_and_column() {
        assert_eq!(
            at("    at Object.<anonymous> (/home/ci/workspace/sample-app/src/app/login.ts:42:13)\n    at run (/home/ci/workspace/sample-app/src/run.js:7:1)"),
            ["/home/ci/workspace/sample-app/src/app/login.ts:42:13", "/home/ci/workspace/sample-app/src/run.js:7:1"]
        );
        assert_eq!(at("    at /work/tests/login.spec.ts:10:5"), ["/work/tests/login.spec.ts:10:5"]);
    }

    #[test]
    fn webpack_and_file_prefixes_and_query_suffixes_are_stripped() {
        assert_eq!(at("at Module.foo (webpack:///./src/components/Button.tsx?2f3a:15:9)"), ["src/components/Button.tsx:15:9"]);
        assert_eq!(at("at x (webpack://app/./src/x.js:3:4)"), ["src/x.js:3:4"]);
        assert_eq!(at("at y (webpack-internal:///./src/y.jsx:8:2)"), ["src/y.jsx:8:2"]);
        assert_eq!(at("at z (file:///Users/me/proj/x.js:10:2)"), ["/Users/me/proj/x.js:10:2"]);
    }

    #[test]
    fn typescript_compiler_style_path_line_col() {
        assert_eq!(at("src/index.ts(12,34): error TS2322: Type 'x' is not assignable"), ["src/index.ts:12:34"]);
    }

    #[test]
    fn python_tracebacks() {
        assert_eq!(
            at("Traceback (most recent call last):\n  File \"/app/tests/test_login.py\", line 57, in test_x\n    assert ok"),
            ["/app/tests/test_login.py:57:null"]
        );
    }

    #[test]
    fn java_frames_become_package_paths_and_are_not_read_twice() {
        assert_eq!(
            at("\tat com.acme.ci.LoginPage.click(LoginPage.java:88)\n\tat org.junit.Assert.fail(Assert.java:89)"),
            ["com/acme/ci/LoginPage.java:88:null", "org/junit/Assert.java:89:null"]
        );
    }

    #[test]
    fn go_panics() {
        assert_eq!(at("main.handler(...)\n\t/go/src/app/handler.go:57 +0x1d"), ["/go/src/app/handler.go:57:null"]);
    }

    #[test]
    fn a_relative_path_with_no_column() {
        assert_eq!(at("Error at tests/login.spec.ts:10"), ["tests/login.spec.ts:10:null"]);
    }

    #[test]
    fn duplicates_are_dropped_and_the_order_of_first_appearance_is_kept() {
        assert_eq!(at("a.ts:1:1 b.ts:2:2 a.ts:1:1"), ["a.ts:1:1", "b.ts:2:2"]);
    }

    #[test]
    fn non_source_files_urls_path_traversal_and_junk_yield_nothing() {
        for text in ["screenshot.png:3", "at https://cdn.example.com/bundle.js:1:2345", "(../../etc/secrets.py:1)", "nothing to see here", "", "   "] {
            assert!(at(text).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn absurd_positions_are_ignored_and_the_text_and_result_count_are_capped() {
        assert!(at("a.ts:0:1 b.ts:99999999999:1").is_empty());
        let many = (0..50).map(|i| format!("f{i}.ts:{}:1", i + 1)).collect::<Vec<_>>().join(" ");
        assert_eq!(parse_locations(&many).len(), 20);
        let huge = format!("{} tail.ts:5:5", "x".repeat(9000));
        assert!(parse_locations(&huge).is_empty(), "beyond the 8 KB cap");
    }

    // Beyond the ported tests: the lookbehind became a consumed boundary
    // character, and `\s`, `\w`, `\d` must stay JS-shaped.
    #[test]
    fn every_boundary_character_starts_a_token_and_neighbours_do_not_swallow_each_other() {
        // Checked against the Node implementation. Separators the path class
        // excludes split the text; `[`, `=` and `@` are boundary characters
        // but also legal path characters, so the leftmost match starts earlier.
        for sep in [' ', '\t', '\n', '(', '"', '\'', '>', ',', ';', '\u{a0}', '\u{feff}'] {
            assert_eq!(at(&format!("x{sep}a/b.ts:3:4")), ["a/b.ts:3:4"], "{sep:?}");
        }
        for sep in ['[', '=', '@'] {
            assert_eq!(at(&format!("x{sep}a/b.ts:3:4")), [format!("x{sep}a/b.ts:3:4")], "{sep:?}");
        }
        assert_eq!(at("a.ts:1:1,b.ts:2:2;c.ts:3:3"), ["a.ts:1:1", "b.ts:2:2", "c.ts:3:3"]);
        assert_eq!(at("(a.ts:1)(b.ts:2)"), ["a.ts:1:null", "b.ts:2:null"]);
        assert_eq!(at("x.ts(1,2),y.ts(3,4)"), ["x.ts:1:2", "y.ts:3:4"]);
        assert!(at("pre/a.ts:3").len() == 1 && at("xa.ts:3")[0] == "xa.ts:3:null", "start of text counts as a boundary");
        assert!(at("foo:bar/a.ts:3").is_empty(), "a colon is not a boundary");
    }

    #[test]
    fn percent_escapes_are_decoded_backslashes_normalized_and_bad_escapes_kept() {
        assert_eq!(at("at x (/w/my%20dir/a.ts:1:1)"), ["/w/my dir/a.ts:1:1"]);
        assert_eq!(at("at x (C:\\proj\\src\\a.ts:1:1)").len(), 0, "a drive letter's colon ends the path");
        assert_eq!(at("at x (src%zz/a.ts:1:1)"), ["src%zz/a.ts:1:1"]);
    }

    #[test]
    fn java_needs_a_package_and_a_capitalized_class() {
        // No package: the Java pattern does not apply, the generic one reads a plain path.
        assert_eq!(at("at Foo.bar(Foo.java:3)"), ["Foo.java:3:null"]);
        assert_eq!(at("at a.b.Foo$Inner.run(Foo.java:3)"), ["a/b/Foo.java:3:null"]);
        assert!(at("format at a.b.Foo.run(Foo.java:3)").len() == 1);
        assert_eq!(at("flat a.b.Foo.run(Foo.java:3)"), ["Foo.java:3:null"], "`at` must be a whole word");
    }
}
