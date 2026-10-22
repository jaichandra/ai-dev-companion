//! Registers the companion's own MCP server ("ai-companion") with the MCP
//! clients that can drive it (port of `core/mcp-registration.js`): Claude Code
//! (`claude mcp add --scope user`) always, and Cursor (`~/.cursor/mcp.json`)
//! only when it's actually installed (optional integrations are gated on
//! detection). Setup calls `register_everywhere` after every run (so a rebuilt
//! port or a rotated `mcp.token` takes effect), and the server calls it again
//! right after a token rotation from the Settings panel. `doctor` uses
//! `registration_status` (read-only, no spawning) to report drift between runs.

use crate::guard::mcp_classifier::COMPANION_MCP_SERVER;
use crate::mcp_auth::{bearer_matches, generate_mcp_token};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

/// `claude mcp add`'s timeout budget for a stuck or slow CLI invocation: this
/// runs synchronously wherever it's called, so a wedged `claude` process must
/// not hang the caller forever.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(20);

/// The URL Claude Code / Cursor should hit for the companion's own MCP server:
/// always loopback, since the service only ever listens on 127.0.0.1.
pub fn mcp_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

/// Removes any existing user-scope entry first, so registration can be re-run (a
/// changed port or a rotated token) without `claude mcp add` refusing to
/// overwrite one that's already there. A missing entry makes this exit non-zero,
/// which callers deliberately ignore.
pub fn claude_remove_args() -> Vec<String> {
    ["mcp", "remove", "--scope", "user", COMPANION_MCP_SERVER].iter().map(|s| s.to_string()).collect()
}

/// `--header` is variadic (`-H, --header <header...>`): it slurps every
/// argument after it, so the name and URL must come first or they'd be
/// swallowed as more headers.
pub fn claude_add_args(port: u16, token: &str) -> Vec<String> {
    ["mcp", "add", "--scope", "user", "--transport", "http", COMPANION_MCP_SERVER].iter().map(|s| s.to_string()).chain([mcp_url(port), "--header".to_string(), format!("Authorization: Bearer {token}")]).collect()
}

const CURSOR_INVALID_JSON_MESSAGE: &str = "~/.cursor/mcp.json isn't valid JSON \u{2014} fix or remove it, then re-run setup.";
const CURSOR_BAD_MCPSERVERS_MESSAGE: &str = "~/.cursor/mcp.json's mcpServers isn't an object \u{2014} fix or remove it, then re-run setup.";

/// True for an absent `mcpServers` or a plain object; arrays and scalars are
/// not something the companion may merge into.
fn is_missing_or_object(v: &Value) -> bool {
    v.is_null() || v.is_object()
}

/// Merges the companion's entry into a Cursor `mcp.json`'s text, preserving
/// every other server and top-level key. `existing_text` is the file's current
/// contents, or `None`/"" if it doesn't exist yet: either produces a fresh file
/// with just this one entry. Errors (rather than silently overwriting) if the
/// text isn't valid JSON: a hand-edited or corrupted `mcp.json` is the user's,
/// and clobbering it would lose whatever else they'd put there.
pub fn merge_cursor_config(existing_text: Option<&str>, port: u16, token: &str) -> Result<String, String> {
    let mut config = Map::new();
    if let Some(text) = existing_text.filter(|t| !t.is_empty()) {
        let parsed: Value = serde_json::from_str(text).map_err(|_| CURSOR_INVALID_JSON_MESSAGE.to_string())?;
        let Some(obj) = parsed.as_object() else { return Err(CURSOR_INVALID_JSON_MESSAGE.into()) };
        // Merging into an array/string/number would silently rewrite the user's value.
        if !is_missing_or_object(&parsed["mcpServers"]) {
            return Err(CURSOR_BAD_MCPSERVERS_MESSAGE.into());
        }
        config = obj.clone();
    }
    let mut servers = config.get("mcpServers").and_then(Value::as_object).cloned().unwrap_or_default();
    servers.insert(COMPANION_MCP_SERVER.into(), json!({ "url": mcp_url(port), "headers": { "Authorization": format!("Bearer {token}") } }));
    config.insert("mcpServers".into(), Value::Object(servers));
    Ok(format!("{}\n", serde_json::to_string_pretty(&Value::Object(config)).expect("a JSON value serializes")))
}

/// One client's config text, parsed for its ai-companion entry: `(entry,
/// unreadable)`. `None` means "no file yet", which is a missing entry, not
/// unreadable: only text that fails to parse counts as unreadable. Both Claude
/// Code's `~/.claude.json` and Cursor's `~/.cursor/mcp.json` keep this at
/// `mcpServers["ai-companion"]`, so one parser covers both.
fn read_entry(json_text: Option<&str>) -> (Option<Value>, bool) {
    let Some(text) = json_text else { return (None, false) };
    match serde_json::from_str::<Value>(text) {
        Ok(p) if p.is_object() && is_missing_or_object(&p["mcpServers"]) => (p["mcpServers"].get(COMPANION_MCP_SERVER).filter(|e| !e.is_null() && e.as_bool() != Some(false)).cloned(), false),
        _ => (None, true),
    }
}

/// One client's registration state given its parsed entry plus the port/token
/// the companion is actually running with right now. Order matters: unreadable
/// and missing both mean there's nothing trustworthy to compare, so they're
/// decided before url/token are even looked at.
fn entry_state(entry: Option<&Value>, unreadable: bool, port: u16, token: &str) -> &'static str {
    if unreadable {
        return "unreadable";
    }
    let Some(e) = entry.filter(|e| e.is_object()) else { return "missing" };
    if e["url"].as_str() != Some(mcp_url(port).as_str()) {
        return "wrong-url";
    }
    if !bearer_matches(e["headers"]["Authorization"].as_str(), Some(token)) {
        return "stale-token";
    }
    "ok"
}

/// Read-only check of whether Claude Code (always) and Cursor (only when
/// `cursor_installed`) are registered with the companion's current port/token:
/// used by `doctor`. Never spawns anything or touches disk itself; the caller
/// reads `~/.claude.json` and, if relevant, `~/.cursor/mcp.json` and hands their
/// raw text in. Returns `(client, state)` pairs.
pub fn registration_status(claude_json_text: Option<&str>, cursor_json_text: Option<&str>, cursor_installed: bool, port: u16, token: &str) -> Vec<(&'static str, &'static str)> {
    let (entry, bad) = read_entry(claude_json_text);
    let mut results = vec![("Claude Code", entry_state(entry.as_ref(), bad, port, token))];
    if cursor_installed {
        let (entry, bad) = read_entry(cursor_json_text);
        results.push(("Cursor", entry_state(entry.as_ref(), bad, port, token)));
    }
    results
}

/// Where the `mcp.token` credential lives.
pub trait McpTokenStore {
    fn get(&self, name: &str) -> Option<String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
}

/// The `mcp.token` credential: whatever's already saved, or a freshly generated
/// one saved on the spot if there's none yet. Returns `(token, created)`.
pub fn ensure_mcp_token(store: &dyn McpTokenStore, generate: &dyn Fn() -> String) -> Result<(String, bool), String> {
    if let Some(existing) = store.get("mcp.token").filter(|t| !t.is_empty()) {
        return Ok((existing, false));
    }
    let token = generate();
    store.set("mcp.token", &token)?;
    Ok((token, true))
}

/// `ensure_mcp_token` with the real generator.
pub fn ensure_mcp_token_default(store: &dyn McpTokenStore) -> Result<(String, bool), String> {
    ensure_mcp_token(store, &generate_mcp_token)
}

/// Strips `token` out of `text` wherever it appears, so a CLI's own error output
/// (which can echo back the failing command line, header included) never ends up
/// carrying the secret into a message that setup prints or the Settings panel renders.
fn redact_token(text: &str, token: &str) -> String {
    if token.is_empty() { text.to_string() } else { text.replace(token, "[redacted]") }
}

/// What a spawned process reported.
#[derive(Debug, Clone, Default)]
pub struct SpawnResult {
    pub status: Option<i32>,
    pub stderr: String,
    /// The spawn itself failed (not found, timed out).
    pub error: Option<String>,
}

/// Runs a program with a timeout, waits for it and reports.
pub type SpawnSync<'a> = &'a dyn Fn(&str, &[String], Duration) -> SpawnResult;

/// The real thing: `std::process::Command`, killed after `timeout`.
pub fn spawn_sync(program: &str, args: &[String], timeout: Duration) -> SpawnResult {
    let mut child = match std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return SpawnResult { error: Some(e.to_string()), ..Default::default() },
    };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = io::Read::read_to_string(&mut e, &mut stderr);
                }
                return SpawnResult { status: status.code(), stderr, error: None };
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return SpawnResult { error: Some(format!("{program} timed out after {} s", timeout.as_secs())), ..Default::default() };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return SpawnResult { error: Some(e.to_string()), ..Default::default() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub ok: bool,
    pub message: String,
}

/// Runs `claude mcp remove` (failure ignored: an absent entry is fine) then
/// `claude mcp add`, and reports the add's outcome.
fn register_with_claude(port: u16, token: &str, spawn: SpawnSync<'_>) -> Registered {
    spawn("claude", &claude_remove_args(), SPAWN_TIMEOUT);
    let add = spawn("claude", &claude_add_args(port, token), SPAWN_TIMEOUT);
    if add.status == Some(0) {
        return Registered { ok: true, message: format!("Registered with Claude Code ({COMPANION_MCP_SERVER}).") };
    }
    let raw = if !add.stderr.is_empty() { add.stderr.clone() } else { add.error.clone().unwrap_or_default() };
    let detail = redact_token(crate::js_text::js_trim(&raw), token);
    Registered { ok: false, message: format!("claude mcp add failed: {}", if detail.is_empty() { "unknown error".to_string() } else { detail }) }
}

/// Merges the companion's entry into `~/.cursor/mcp.json` and writes it back
/// atomically (temp file + rename) at mode 0600, since this file now carries a
/// bearer token too.
fn register_with_cursor(port: u16, token: &str, home: &Path) -> Registered {
    let file = home.join(".cursor").join("mcp.json");
    let fail = |message: String| Registered { ok: false, message: redact_token(&message, token) };
    let existing = match fs::read(&file) {
        Ok(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return fail(e.to_string()),
    };
    let next = match merge_cursor_config(existing.as_deref(), port, token) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let tmp = home.join(".cursor").join(format!(".mcp.json.{}.{stamp}.tmp", std::process::id()));
    let written = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(next.as_bytes())?;
        drop(f);
        fs::rename(&tmp, &file)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return fail(e.to_string());
    }
    Registered { ok: true, message: "Registered with Cursor (~/.cursor/mcp.json).".into() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registrations {
    pub claude: Option<Registered>,
    pub cursor: Option<Registered>,
}

/// Registers the companion's MCP server with every client that's actually
/// present on this machine (optional integrations are gated on detection):
/// Claude Code only when `claude_available` (checked by the caller), Cursor only
/// when `~/.cursor` exists. Never puts the token in any message, since both
/// setup's console output and the Settings panel show these as-is.
pub fn register_everywhere(port: u16, token: &str, home: &Path, claude_available: bool, spawn: SpawnSync<'_>) -> Registrations {
    let claude = claude_available.then(|| register_with_claude(port, token, spawn));
    let cursor = home.join(".cursor").exists().then(|| register_with_cursor(port, token, home));
    Registrations { claude, cursor }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    type Calls = std::rc::Rc<RefCell<Vec<(String, Vec<String>, Duration)>>>;

    fn fake(results: Vec<SpawnResult>) -> (Calls, impl Fn(&str, &[String], Duration) -> SpawnResult) {
        let calls: Calls = Default::default();
        let recorded = calls.clone();
        let queue = RefCell::new(results);
        let f = move |c: &str, a: &[String], t: Duration| {
            recorded.borrow_mut().push((c.to_string(), a.to_vec(), t));
            let mut q = queue.borrow_mut();
            if q.len() > 1 { q.remove(0) } else { q[0].clone() }
        };
        (calls, f)
    }

    fn ok() -> SpawnResult {
        SpawnResult { status: Some(0), ..Default::default() }
    }

    fn no_spawn(_: &str, _: &[String], _: Duration) -> SpawnResult {
        panic!("should not spawn")
    }

    #[test]
    fn urls_and_claude_args() {
        assert_eq!(mcp_url(8787), "http://127.0.0.1:8787/mcp");
        assert_eq!(claude_remove_args(), ["mcp", "remove", "--scope", "user", "ai-companion"]);
        assert_eq!(
            claude_add_args(8787, "tok-value"),
            ["mcp", "add", "--scope", "user", "--transport", "http", "ai-companion", "http://127.0.0.1:8787/mcp", "--header", "Authorization: Bearer tok-value"]
        );
    }

    #[test]
    fn merge_cursor_config_keeps_other_servers_and_unrelated_top_level_keys() {
        let existing = json!({ "mcpServers": { "other": { "url": "http://example.test/mcp" } }, "someOtherKey": "kept" }).to_string();
        let parsed: Value = serde_json::from_str(&merge_cursor_config(Some(&existing), 8787, "tok").unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["other"], json!({ "url": "http://example.test/mcp" }));
        assert_eq!(parsed["someOtherKey"], "kept");
        assert_eq!(parsed["mcpServers"]["ai-companion"], json!({ "url": "http://127.0.0.1:8787/mcp", "headers": { "Authorization": "Bearer tok" } }));
    }

    #[test]
    fn merge_cursor_config_produces_a_fresh_file_for_none_or_empty_and_is_idempotent() {
        for input in [None, Some("")] {
            let parsed: Value = serde_json::from_str(&merge_cursor_config(input, 8787, "tok").unwrap()).unwrap();
            assert_eq!(parsed, json!({ "mcpServers": { "ai-companion": { "url": "http://127.0.0.1:8787/mcp", "headers": { "Authorization": "Bearer tok" } } } }));
        }
        let once = merge_cursor_config(None, 8787, "tok").unwrap();
        assert_eq!(merge_cursor_config(Some(&once), 8787, "tok").unwrap(), once);
        assert!(once.ends_with("}\n"));
    }

    #[test]
    fn merge_cursor_config_errors_on_unparsable_input_and_on_a_non_object_mcp_servers() {
        assert_eq!(merge_cursor_config(Some("{not json"), 8787, "tok").unwrap_err(), "~/.cursor/mcp.json isn't valid JSON \u{2014} fix or remove it, then re-run setup.");
        for text in ["[1]", "5", "null"] {
            assert!(merge_cursor_config(Some(text), 8787, "t").unwrap_err().contains("isn't valid JSON"), "{text}");
        }
        for bad in [json!(["x"]), json!("oops"), json!(5)] {
            let e = merge_cursor_config(Some(&json!({ "mcpServers": bad }).to_string()), 8787, "t").unwrap_err();
            assert!(e.contains("mcp.json") && e.contains("fix or remove it, then re-run setup"), "{e}");
            let r = registration_status(None, Some(&json!({ "mcpServers": bad }).to_string()), true, 8787, "t");
            assert_eq!(r.iter().find(|(c, _)| *c == "Cursor").unwrap().1, "unreadable");
        }
    }

    fn claude_json(url: &str, auth: &str) -> String {
        json!({ "mcpServers": { "ai-companion": { "type": "http", "url": url, "headers": { "Authorization": auth } } } }).to_string()
    }

    #[test]
    fn registration_status_reports_each_state() {
        let status = |text: Option<&str>, port, token| registration_status(text, None, false, port, token);
        assert_eq!(status(Some(&claude_json(&mcp_url(8787), "Bearer tok")), 8787, "tok"), [("Claude Code", "ok")]);
        assert_eq!(status(Some("{}"), 8787, "tok"), [("Claude Code", "missing")]);
        assert_eq!(status(None, 8787, "tok"), [("Claude Code", "missing")]);
        assert_eq!(status(Some(&claude_json(&mcp_url(8787), "Bearer old-token")), 8787, "new-token"), [("Claude Code", "stale-token")]);
        assert_eq!(status(Some(&claude_json(&mcp_url(9999), "Bearer tok")), 8787, "tok"), [("Claude Code", "wrong-url")]);
        assert_eq!(status(Some("{not json"), 8787, "tok"), [("Claude Code", "unreadable")]);
    }

    #[test]
    fn registration_status_includes_cursor_only_when_installed() {
        let cursor = json!({ "mcpServers": { "ai-companion": { "url": mcp_url(8787), "headers": { "Authorization": "Bearer tok" } } } }).to_string();
        assert_eq!(registration_status(None, Some(&cursor), false, 8787, "tok"), [("Claude Code", "missing")]);
        assert_eq!(registration_status(None, Some(&cursor), true, 8787, "tok"), [("Claude Code", "missing"), ("Cursor", "ok")]);
    }

    struct Store(RefCell<Vec<(String, String)>>, Option<String>);
    impl McpTokenStore for Store {
        fn get(&self, name: &str) -> Option<String> {
            (name == "mcp.token").then(|| self.1.clone()).flatten()
        }
        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            self.0.borrow_mut().push((name.into(), value.into()));
            Ok(())
        }
    }

    #[test]
    fn ensure_mcp_token_keeps_an_existing_token_or_creates_and_saves_exactly_one() {
        let existing = Store(RefCell::new(vec![]), Some("existing-token".into()));
        assert_eq!(ensure_mcp_token(&existing, &|| "unused".into()).unwrap(), ("existing-token".to_string(), false));
        assert!(existing.0.borrow().is_empty(), "nothing saved when a token already exists");
        let empty = Store(RefCell::new(vec![]), None);
        assert_eq!(ensure_mcp_token(&empty, &|| "generated-token".into()).unwrap(), ("generated-token".to_string(), true));
        assert_eq!(*empty.0.borrow(), [("mcp.token".to_string(), "generated-token".to_string())]);
        let (token, created) = ensure_mcp_token_default(&Store(RefCell::new(vec![]), None)).unwrap();
        assert!(created && token.len() >= 32);
    }

    #[test]
    fn register_everywhere_skips_claude_when_unavailable_and_cursor_when_there_is_no_dot_cursor() {
        let home = tempfile::tempdir().unwrap();
        let r = register_everywhere(8787, "tok-value", home.path(), false, &no_spawn);
        assert_eq!(r, Registrations { claude: None, cursor: None });
    }

    #[test]
    fn register_everywhere_runs_remove_then_add_ignoring_a_failing_remove_with_the_documented_timeout() {
        let home = tempfile::tempdir().unwrap();
        let (calls, spawn) = fake(vec![SpawnResult { status: Some(1), stderr: "no such server".into(), error: None }, ok()]);
        let r = register_everywhere(8787, "tok-value", home.path(), true, &spawn);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0.as_str(), &calls[0].1), ("claude", &claude_remove_args()));
        assert_eq!((calls[1].0.as_str(), &calls[1].1), ("claude", &claude_add_args(8787, "tok-value")));
        assert!(calls.iter().all(|c| c.2 == Duration::from_secs(20)));
        let claude = r.claude.unwrap();
        assert!(claude.ok && claude.message == "Registered with Claude Code (ai-companion)." && !claude.message.contains("tok-value"));
    }

    #[test]
    fn register_everywhere_redacts_the_token_from_a_failing_adds_message() {
        let token = "super-secret-token-value";
        let home = tempfile::tempdir().unwrap();
        let (_, spawn) = fake(vec![ok(), SpawnResult { status: Some(1), stderr: format!("  error: invalid header \"Authorization: Bearer {token}\"\n"), error: None }]);
        let r = register_everywhere(8787, token, home.path(), true, &spawn).claude.unwrap();
        assert!(!r.ok && !r.message.contains(token) && r.message.contains("[redacted]"), "{}", r.message);
        let (_, quiet) = fake(vec![ok(), SpawnResult { status: Some(1), ..Default::default() }]);
        assert_eq!(register_everywhere(8787, token, home.path(), true, &quiet).claude.unwrap().message, "claude mcp add failed: unknown error");
        let (_, broken) = fake(vec![ok(), SpawnResult { status: None, stderr: String::new(), error: Some("spawn claude ENOENT".into()) }]);
        assert_eq!(register_everywhere(8787, token, home.path(), true, &broken).claude.unwrap().message, "claude mcp add failed: spawn claude ENOENT");
    }

    #[test]
    fn register_everywhere_writes_the_cursor_file_at_mode_0600_merging_other_servers() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".cursor")).unwrap();
        let file = home.path().join(".cursor").join("mcp.json");
        let r = register_everywhere(8787, "tok-value", home.path(), false, &no_spawn).cursor.unwrap();
        assert!(r.ok && !r.message.contains("tok-value"));
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let written: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["ai-companion"], json!({ "url": mcp_url(8787), "headers": { "Authorization": "Bearer tok-value" } }));
        // A second registration keeps what else is there.
        fs::write(&file, json!({ "mcpServers": { "other": { "url": "http://example.test/mcp" } } }).to_string()).unwrap();
        assert!(register_everywhere(8787, "tok-value", home.path(), false, &no_spawn).cursor.unwrap().ok);
        let merged: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(merged["mcpServers"]["other"], json!({ "url": "http://example.test/mcp" }));
        assert_eq!(merged["mcpServers"]["ai-companion"]["url"], mcp_url(8787));
        assert_eq!(fs::read_dir(home.path().join(".cursor")).unwrap().count(), 1, "no temp file is left behind");
    }

    #[test]
    fn register_everywhere_leaves_a_cursor_config_with_a_non_object_mcp_servers_byte_for_byte_unchanged() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".cursor")).unwrap();
        let file = home.path().join(".cursor").join("mcp.json");
        let original = r#"{"mcpServers":["keep","me"],"other":1}"#;
        fs::write(&file, original).unwrap();
        let r = register_everywhere(8787, "tok-value", home.path(), false, &no_spawn).cursor.unwrap();
        assert!(!r.ok);
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn spawn_sync_reports_status_stderr_and_a_missing_program_and_kills_a_hung_one() {
        let r = spawn_sync("/bin/sh", &["-c".into(), "echo oops >&2; exit 3".into()], Duration::from_secs(5));
        assert_eq!((r.status, r.stderr.trim()), (Some(3), "oops"));
        assert!(spawn_sync("/definitely/not/a/program", &[], Duration::from_secs(1)).error.is_some());
        let started = std::time::Instant::now();
        let hung = spawn_sync("/bin/sh", &["-c".into(), "sleep 5".into()], Duration::from_millis(200));
        assert!(hung.error.unwrap().contains("timed out") && started.elapsed() < Duration::from_secs(3));
    }
}
