//! Pure decision logic for the edit and read PreToolUse hooks (port of
//! `core/edit-path-policy.js`). Does real filesystem resolution: given one
//! allowed root, decide whether a candidate path is really inside it
//! (following symlinks) and doesn't touch a `.git` path anywhere.
//!
//! Paths are POSIX-style, as in Node's `path` on macOS/Linux.

use crate::js_text::js_trim;
use serde_json::Value;
use std::fs::{self, Metadata};
use std::io::ErrorKind;

/// The result of [`resolve_path_safely`].
pub struct Resolved {
    pub real: String,
    pub final_is_symlink: bool,
    /// lstat of the LAST component, before any symlink is followed; `None`
    /// for a not-yet-existing target.
    pub final_stat: Option<Metadata>,
}

fn is_absolute(p: &str) -> bool {
    p.starts_with('/')
}

fn dirname(p: &str) -> String {
    match p.rfind('/') {
        None | Some(0) => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

fn join_name(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// Resolves `abs_path` one component at a time, with `lstat`, the way the OS
/// itself will when the tool call opens it. `None` when the path can't be
/// resolved safely.
///
/// Starting at `/`:
///  - `.` is skipped and `..` takes the dirname of the (already real) current
///    directory: physical, not lexical, resolution;
///  - a symlink component is resolved with `canonicalize`; if that fails
///    (dangling, a loop, a permission error) the whole path is unresolvable;
///  - the first component that doesn't exist ends the walk: it and everything
///    after it are plain names under a real directory, so they're rejoined
///    as-is (after refusing any `..` among them);
///  - any other lstat error (ENOTDIR, EACCES, ...) is unresolvable too.
pub fn resolve_path_safely(abs_path: &str) -> Option<Resolved> {
    let parts: Vec<&str> = abs_path.split('/').filter(|p| !p.is_empty()).collect();
    let mut cur = "/".to_string();
    let mut final_is_symlink = false;
    let mut final_stat = None;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        if *part == "." {
            continue;
        }
        if *part == ".." {
            cur = dirname(&cur);
            continue;
        }
        let next = join_name(&cur, part);
        let st = match fs::symlink_metadata(&next) {
            Ok(st) => st,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let rest = &parts[i..];
                if rest.contains(&"..") {
                    return None;
                }
                let real = rest.iter().filter(|p| **p != ".").fold(cur, |acc, p| join_name(&acc, p));
                return Some(Resolved { real, final_is_symlink: false, final_stat: None });
            }
            Err(_) => return None,
        };
        if st.file_type().is_symlink() {
            cur = fs::canonicalize(&next).ok()?.to_str()?.to_string();
            if is_last {
                final_is_symlink = true;
            }
        } else {
            cur = next;
        }
        if is_last {
            final_stat = Some(st);
        }
    }
    Some(Resolved { real: cur, final_is_symlink, final_stat })
}

/// `path.resolve` for an already-absolute or cwd-relative path, as components.
fn normalized_components(p: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let absolute = if is_absolute(p) {
        p.to_string()
    } else {
        let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
        format!("{cwd}/{p}")
    };
    for c in absolute.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            _ => out.push(c.to_string()),
        }
    }
    out
}

/// True if `abs_path` is STRICTLY nested inside `root` (not equal to it:
/// `root` itself is a directory, never a valid Edit/Write target). Compares
/// whole components, not a string prefix, so `/foo-bar` is NOT inside `/foo`.
pub fn is_strictly_inside(root: &str, abs_path: &str) -> bool {
    let r = normalized_components(root);
    let p = normalized_components(abs_path);
    p.len() > r.len() && p[..r.len()] == r[..]
}

/// True if any path segment of `abs_path` is `.git`, case-INsensitively:
/// covers the worktree's own gitlink FILE and anything under a `.git`
/// directory anywhere inside the root. Case-insensitive because APFS is
/// case-insensitive-but-case-preserving, so `<root>/.GIT` resolves to itself
/// unchanged.
pub fn touches_git_path(abs_path: &str) -> bool {
    abs_path.split('/').any(|segment| segment.to_lowercase() == ".git")
}

#[cfg(unix)]
fn has_other_hard_links(st: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    st.nlink() > 1
}

#[cfg(not(unix))]
fn has_other_hard_links(_st: &Metadata) -> bool {
    false
}

/// Decides whether an Edit/Write/MultiEdit/NotebookEdit call targeting
/// `raw_path` may proceed, given `root` (an absolute path the caller should
/// already have resolved to a real path). Allows ONLY when the resolved
/// candidate is strictly inside `root` and doesn't touch any `.git` segment.
///
/// On top of that the target itself must not be a way to write elsewhere:
///  - any symlink in the path that can't be resolved denies;
///  - the final component being a symlink AT ALL denies, dangling or live;
///  - an existing non-directory target with `nlink > 1` denies: a hardlink
///    shares its inode with another name that may live outside the root.
///
/// `raw_path` must already be absolute, whitespace-trimmed and not
/// `~`-prefixed, exactly what Claude Code hands a PreToolUse hook. Anything
/// that doesn't look like a real absolute path is denied outright.
pub fn is_edit_allowed(root: &str, raw_path: &str) -> bool {
    if root.is_empty() || raw_path.is_empty() {
        return false;
    }
    if raw_path != js_trim(raw_path) || raw_path.starts_with('~') || !is_absolute(raw_path) {
        return false;
    }
    let Some(resolved) = resolve_path_safely(raw_path) else { return false };
    if resolved.final_is_symlink {
        return false;
    }
    if let Some(st) = &resolved.final_stat {
        if !st.is_dir() && has_other_hard_links(st) {
            return false;
        }
    }
    is_strictly_inside(root, &resolved.real) && !touches_git_path(&resolved.real)
}

/// Decides whether a Read/Glob/Grep call may look at `raw_path`, given the
/// same real-path `root`. Same symlink-safe resolution, but WITHOUT the
/// edit-only rules: reading inside `.git` and reading a hardlinked file are
/// harmless, and `root` itself is a valid search path. A relative candidate is
/// resolved against `root`; a leading `~` or surrounding whitespace denies.
pub fn is_read_allowed(root: &str, raw_path: &str) -> bool {
    if root.is_empty() || raw_path.is_empty() {
        return false;
    }
    if raw_path != js_trim(raw_path) || raw_path.starts_with('~') {
        return false;
    }
    // Not a lexical normalize: that would apply `..` before any symlink is
    // looked at; resolve_path_safely does it physically.
    let candidate = if is_absolute(raw_path) { raw_path.to_string() } else { format!("{root}/{raw_path}") };
    match resolve_path_safely(&candidate) {
        Some(r) => r.real == root || is_strictly_inside(root, &r.real),
        None => false,
    }
}

/// True if a Glob `pattern` could reach outside the search directory on its
/// own: absolute, `~`-prefixed, or with a `..` segment (either separator).
/// `..` inside a name (`a..b`) is fine.
pub fn glob_pattern_escapes(pattern: &str) -> bool {
    pattern.starts_with('~') || is_absolute(pattern) || pattern.split(['\\', '/']).any(|s| s == "..")
}

/// The read guard's whole decision for one PreToolUse payload: `tool_name` and
/// its `tool_input` exactly as Claude Code sends them. Read's target is
/// `file_path`; Glob's and Grep's is their optional `path` (absent, null or
/// empty means the run's cwd, which is `root`). Glob's `pattern` is also
/// checked. Any other tool name, a missing Read `file_path` or a non-string
/// field denies.
pub fn is_read_tool_call_allowed(root: &str, tool_name: &str, tool_input: &Value) -> bool {
    // JS `typeof x === "object"` accepts arrays (with no named fields) too.
    let field = |name: &str| match tool_input {
        Value::Object(map) => map.get(name),
        _ => None,
    };
    if !matches!(tool_input, Value::Object(_) | Value::Array(_)) {
        return false;
    }
    let optional_path_allowed = || match field("path") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) if s.is_empty() => true,
        Some(Value::String(s)) => is_read_allowed(root, s),
        Some(_) => false,
    };
    match tool_name {
        "Read" => matches!(field("file_path"), Some(Value::String(p)) if is_read_allowed(root, p)),
        "Glob" => match field("pattern") {
            Some(Value::String(p)) if !glob_pattern_escapes(p) => optional_path_allowed(),
            _ => false,
        },
        "Grep" => optional_path_allowed(),
        _ => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;

    /// A fresh root dir under the real temp dir, itself canonicalized: on
    /// macOS the temp dir is under `/var/folders/...`, itself a symlink to
    /// `/private/var/folders/...`.
    struct Fixture {
        _guard: TempDir,
        root: String,
    }

    fn make_root() -> Fixture {
        let guard = tempfile::Builder::new().prefix("edit-path-policy-test-").tempdir().unwrap();
        let root = fs::canonicalize(guard.path()).unwrap().to_str().unwrap().to_string();
        Fixture { _guard: guard, root }
    }

    fn outside_dir() -> TempDir {
        tempfile::Builder::new().prefix("edit-path-policy-outside-").tempdir().unwrap()
    }

    fn p(root: &str, rel: &str) -> String {
        format!("{root}/{rel}")
    }

    fn s(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn edit_a_file_inside_root_is_allowed() {
        let f = make_root();
        let file = p(&f.root, "a.txt");
        fs::write(&file, "hi").unwrap();
        assert!(is_edit_allowed(&f.root, &file));
    }

    #[test]
    fn edit_a_new_file_that_does_not_exist_yet_inside_root_is_allowed() {
        let f = make_root();
        assert!(is_edit_allowed(&f.root, &p(&f.root, "brand-new.txt")));
    }

    #[test]
    fn edit_the_root_directory_itself_is_denied() {
        let f = make_root();
        assert!(!is_edit_allowed(&f.root, &f.root));
    }

    #[test]
    fn edit_root_dot_git_gitlink_file_is_denied() {
        let f = make_root();
        let gitlink = p(&f.root, ".git");
        fs::write(&gitlink, "gitdir: /somewhere\n").unwrap();
        assert!(!is_edit_allowed(&f.root, &gitlink));
    }

    #[test]
    fn edit_anything_under_a_nested_dot_git_directory_is_denied() {
        let f = make_root();
        fs::create_dir_all(p(&f.root, ".git/hooks")).unwrap();
        assert!(!is_edit_allowed(&f.root, &p(&f.root, ".git/hooks/pre-commit")));
    }

    // APFS is case-insensitive-but-case-preserving, so an exact ".git" string
    // comparison lets a differently-cased gitlink/gitdir through. The check is
    // on the string, so it holds on any filesystem.
    #[test]
    fn edit_differently_cased_dot_git_is_denied() {
        for name in [".GIT", ".Git"] {
            let f = make_root();
            let gitlink = p(&f.root, name);
            fs::write(&gitlink, "gitdir: /somewhere\n").unwrap();
            assert!(!is_edit_allowed(&f.root, &gitlink), "{name}");
        }
    }

    #[test]
    fn edit_anything_under_a_nested_differently_cased_dot_git_directory_is_denied() {
        let f = make_root();
        fs::create_dir_all(p(&f.root, "sub/.GIT/hooks")).unwrap();
        assert!(!is_edit_allowed(&f.root, &p(&f.root, "sub/.GIT/hooks/pre-commit")));
    }

    #[test]
    fn edit_a_dot_dot_escape_out_of_root_is_denied() {
        let f = make_root();
        assert!(!is_edit_allowed(&f.root, &p(&f.root, "../escaped.txt")));
    }

    #[test]
    fn edit_a_symlink_inside_root_pointing_outside_root_is_denied() {
        let f = make_root();
        let out = outside_dir();
        let secret = out.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        let link = p(&f.root, "link.txt");
        symlink(&secret, &link).unwrap();
        assert!(!is_edit_allowed(&f.root, &link));
    }

    #[test]
    fn edit_an_absolute_path_elsewhere_sibling_of_root_is_denied() {
        let f = make_root();
        let sibling = outside_dir();
        let file = sibling.path().join("outside.txt");
        fs::write(&file, "x").unwrap();
        assert!(!is_edit_allowed(&f.root, &s(&file)));
    }

    #[test]
    fn edit_a_relative_candidate_path_is_denied_outright_not_resolved_against_root() {
        let f = make_root();
        fs::write(p(&f.root, "rel.txt"), "x").unwrap();
        assert!(!is_edit_allowed(&f.root, "rel.txt"));
        assert!(!is_edit_allowed(&f.root, "../escaped-rel.txt"));
    }

    #[test]
    fn edit_a_leading_tilde_is_denied() {
        let f = make_root();
        for raw in ["~/.zshrc", "~", "~root/.zshrc"] {
            assert!(!is_edit_allowed(&f.root, raw), "{raw}");
        }
    }

    #[test]
    fn edit_leading_or_trailing_whitespace_on_the_candidate_is_denied() {
        let f = make_root();
        let file = p(&f.root, "a.txt");
        fs::write(&file, "hi").unwrap();
        assert!(!is_edit_allowed(&f.root, &format!(" {file}")));
        assert!(!is_edit_allowed(&f.root, &format!("{file} ")));
        assert!(!is_edit_allowed(&f.root, &format!("{file}\n")));
        // U+FEFF is JS whitespace, so it counts here too.
        assert!(!is_edit_allowed(&f.root, &format!("{file}\u{FEFF}")));
    }

    #[test]
    fn edit_a_clean_absolute_in_root_path_is_still_allowed() {
        let f = make_root();
        let file = p(&f.root, "a.txt");
        fs::write(&file, "hi").unwrap();
        assert!(is_edit_allowed(&f.root, &file));
    }

    #[test]
    fn edit_denies_empty_inputs() {
        let f = make_root();
        assert!(!is_edit_allowed(&f.root, ""));
        assert!(!is_edit_allowed("", &p(&f.root, "a.txt")));
    }

    #[test]
    fn edit_a_dangling_symlink_as_the_final_component_is_denied() {
        let f = make_root();
        let out = outside_dir();
        let link = p(&f.root, "link");
        symlink(out.path().join("newfile.txt"), &link).unwrap();
        assert!(!is_edit_allowed(&f.root, &link));
    }

    #[test]
    fn edit_a_dangling_relative_symlink_is_denied() {
        let f = make_root();
        let link = p(&f.root, "link");
        symlink("../outside-new.txt", &link).unwrap();
        assert!(!is_edit_allowed(&f.root, &link));
    }

    #[test]
    fn edit_a_dangling_symlink_as_an_intermediate_directory_is_denied() {
        let f = make_root();
        let out = outside_dir();
        let dir_link = p(&f.root, "dirlink");
        symlink(out.path().join("not-yet"), &dir_link).unwrap();
        assert!(!is_edit_allowed(&f.root, &format!("{dir_link}/file.txt")));
    }

    #[test]
    fn edit_a_live_symlink_as_the_final_component_is_denied_even_when_it_points_inside_root() {
        let f = make_root();
        let target = p(&f.root, "target.txt");
        fs::write(&target, "x").unwrap();
        let link = p(&f.root, "link.txt");
        symlink(&target, &link).unwrap();
        assert!(!is_edit_allowed(&f.root, &link));
        // The real file itself stays editable.
        assert!(is_edit_allowed(&f.root, &target));
    }

    #[test]
    fn edit_a_live_intermediate_directory_symlink_that_stays_inside_root_is_allowed() {
        let f = make_root();
        fs::create_dir(p(&f.root, "real")).unwrap();
        symlink(p(&f.root, "real"), p(&f.root, "alias")).unwrap();
        assert!(is_edit_allowed(&f.root, &p(&f.root, "alias/new.txt")));
    }

    #[test]
    fn edit_a_hardlinked_file_is_denied_it_may_share_its_inode_with_a_file_outside_root() {
        let f = make_root();
        let out = outside_dir();
        let victim = out.path().join("victim.txt");
        fs::write(&victim, "x").unwrap();
        let inside = p(&f.root, "hard.txt");
        fs::hard_link(&victim, &inside).unwrap();
        assert!(!is_edit_allowed(&f.root, &inside));
    }

    #[test]
    fn edit_a_dot_dot_that_walks_through_a_symlinked_dir_is_resolved_physically_not_lexically() {
        let f = make_root();
        let out = outside_dir();
        fs::create_dir(out.path().join("deep")).unwrap();
        // root/in -> outside/deep ; root/in/../x is physically outside/x.
        symlink(out.path().join("deep"), p(&f.root, "in")).unwrap();
        assert!(!is_edit_allowed(&f.root, &format!("{}/in/../x.txt", f.root)));
    }

    #[test]
    fn edit_normal_file_and_new_file_in_a_new_subdir_still_allowed_after_the_symlink_hardening() {
        let f = make_root();
        let file = p(&f.root, "a.txt");
        fs::write(&file, "hi").unwrap();
        assert!(is_edit_allowed(&f.root, &file));
        assert!(is_edit_allowed(&f.root, &p(&f.root, "new-dir/deeper/b.txt")));
    }

    #[test]
    fn read_in_root_file_root_itself_dot_git_and_a_hardlinked_file_are_all_readable() {
        let f = make_root();
        let file = p(&f.root, "a.txt");
        fs::write(&file, "hi").unwrap();
        fs::create_dir(p(&f.root, ".git")).unwrap();
        fs::write(p(&f.root, ".git/HEAD"), "ref: x\n").unwrap();
        let out = outside_dir();
        fs::write(out.path().join("h.txt"), "x").unwrap();
        fs::hard_link(out.path().join("h.txt"), p(&f.root, "h.txt")).unwrap();
        assert!(is_read_allowed(&f.root, &file));
        assert!(is_read_allowed(&f.root, &f.root));
        assert!(is_read_allowed(&f.root, &p(&f.root, ".git/HEAD")));
        assert!(is_read_allowed(&f.root, &p(&f.root, "h.txt")));
    }

    #[test]
    fn read_relative_paths_resolve_against_root_escapes_tilde_and_whitespace_are_denied() {
        let f = make_root();
        fs::create_dir(p(&f.root, "src")).unwrap();
        assert!(is_read_allowed(&f.root, "src"));
        assert!(is_read_allowed(&f.root, "."));
        assert!(!is_read_allowed(&f.root, ".."));
        assert!(!is_read_allowed(&f.root, "../sibling/x"));
        assert!(!is_read_allowed(&f.root, "~/.ssh/id_rsa"));
        assert!(!is_read_allowed(&f.root, " src"));
        assert!(!is_read_allowed(&f.root, ""));
    }

    #[test]
    fn read_outside_root_symlink_out_dangling_denied_in_root_live_link_allowed() {
        let f = make_root();
        let out = outside_dir();
        let secret = out.path().join("secret.txt");
        fs::write(&secret, "s").unwrap();
        assert!(!is_read_allowed(&f.root, &s(&secret)));
        symlink(&secret, p(&f.root, "out-link")).unwrap();
        assert!(!is_read_allowed(&f.root, &p(&f.root, "out-link")));
        symlink(out.path().join("missing"), p(&f.root, "dangling")).unwrap();
        assert!(!is_read_allowed(&f.root, &p(&f.root, "dangling")));
        fs::write(p(&f.root, "t.txt"), "t").unwrap();
        symlink(p(&f.root, "t.txt"), p(&f.root, "in-link")).unwrap();
        assert!(is_read_allowed(&f.root, &p(&f.root, "in-link")));
    }

    #[test]
    fn read_tool_call_read_checks_file_path_missing_file_path_denies() {
        let f = make_root();
        fs::write(p(&f.root, "a.txt"), "x").unwrap();
        assert!(is_read_tool_call_allowed(&f.root, "Read", &json!({ "file_path": p(&f.root, "a.txt") })));
        assert!(!is_read_tool_call_allowed(&f.root, "Read", &json!({ "file_path": "/etc/hosts" })));
        assert!(!is_read_tool_call_allowed(&f.root, "Read", &json!({})));
        assert!(!is_read_tool_call_allowed(&f.root, "Read", &json!(null)));
    }

    #[test]
    fn read_tool_call_glob_grep_with_no_path_allowed_path_outside_denied() {
        let f = make_root();
        fs::create_dir(p(&f.root, "src")).unwrap();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        for tool in ["Glob", "Grep"] {
            let call = |input: Value| is_read_tool_call_allowed(&f.root, tool, &input);
            assert!(call(json!({ "pattern": "**/*.js" })), "{tool}");
            assert!(call(json!({ "pattern": "x", "path": "" })), "{tool}");
            assert!(call(json!({ "pattern": "x", "path": f.root })), "{tool}");
            assert!(call(json!({ "pattern": "x", "path": "src" })), "{tool}");
            assert!(call(json!({ "pattern": "x", "path": p(&f.root, "src") })), "{tool}");
            assert!(!call(json!({ "pattern": "x", "path": home })), "{tool}");
            assert!(!call(json!({ "pattern": "x", "path": ".." })), "{tool}");
            assert!(!call(json!({ "pattern": "x", "path": 42 })), "{tool}");
        }
    }

    #[test]
    fn read_tool_call_a_glob_pattern_that_is_absolute_tilde_or_has_a_dot_dot_segment_is_denied() {
        let f = make_root();
        let glob = |pattern: Value| is_read_tool_call_allowed(&f.root, "Glob", &json!({ "pattern": pattern }));
        assert!(!glob(json!("/etc/*")));
        assert!(!glob(json!("~/.ssh/*")));
        assert!(!glob(json!("../*")));
        assert!(!glob(json!("src/../../x/*")));
        assert!(!glob(json!("src\\..\\..\\x")));
        assert!(!is_read_tool_call_allowed(&f.root, "Glob", &json!({})));
        // `..` only as part of a name is fine.
        assert!(glob(json!("**/a..b.txt")));
    }

    #[test]
    fn read_tool_call_an_unknown_tool_name_denies_fail_closed() {
        let f = make_root();
        assert!(!is_read_tool_call_allowed(&f.root, "WebFetch", &json!({ "url": "x" })));
        assert!(!is_read_tool_call_allowed(&f.root, "", &json!({})));
    }

    // Beyond the ported tests.
    #[test]
    fn strictly_inside_compares_whole_components() {
        assert!(is_strictly_inside("/foo", "/foo/bar"));
        assert!(!is_strictly_inside("/foo", "/foo"));
        assert!(!is_strictly_inside("/foo", "/foo-bar/x"));
        assert!(!is_strictly_inside("/foo/bar", "/foo"));
        assert!(is_strictly_inside("/", "/anything"));
        assert!(!is_strictly_inside("/foo", "/foo/../x"));
    }

    #[test]
    fn a_path_ending_in_a_dot_segment_has_no_final_stat() {
        let f = make_root();
        // As in the JS walk, a trailing `.` is skipped before the last-component
        // bookkeeping, so the (real) directory before it never becomes the
        // final stat.
        let r = resolve_path_safely(&format!("{}/.", f.root)).unwrap();
        assert_eq!(r.real, f.root);
        assert!(r.final_stat.is_none());
        assert!(!r.final_is_symlink);
    }
}
