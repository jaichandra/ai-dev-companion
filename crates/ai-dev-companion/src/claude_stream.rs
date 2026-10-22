//! Reads `claude -p --output-format stream-json --verbose` output (one JSON
//! object per line) and turns tool calls into short progress labels for the
//! panel, plus the final result (port of `core/claude-stream.js`). Labels come
//! from Claude's own tool inputs, so they are cut to one line of at most 80
//! characters, and WebFetch/WebSearch labels never include the URL.

use crate::js_text::{is_js_space, js_to_string, slice_utf16, utf16_len};
use serde_json::Value;

const LABEL_MAX: usize = 80;

/// Control characters become spaces, whitespace runs collapse, the result is
/// trimmed and cut to 80 UTF-16 units with an ellipsis.
fn clean(text: &str) -> String {
    let spaced: String = text.chars().map(|c| if c <= '\u{1f}' || c == '\u{7f}' { ' ' } else { c }).collect();
    let one_line = spaced.split(is_js_space).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
    if utf16_len(&one_line) > LABEL_MAX { format!("{}\u{2026}", slice_utf16(&one_line, LABEL_MAX - 1)) } else { one_line }
}

/// The line as a typed JSON object (it has a string `type`), else `None`.
pub fn parse_stream_line(line: &str) -> Option<Value> {
    if !line.starts_with('{') {
        return None;
    }
    let event: Value = serde_json::from_str(line).ok()?;
    event.get("type").is_some_and(Value::is_string).then_some(event)
}

fn short_path(p: Option<&Value>, cwd: Option<&str>) -> String {
    let Some(Value::String(p)) = p.filter(|p| p.as_str().is_some_and(|s| !s.is_empty())) else { return String::new() };
    match cwd {
        Some(cwd) if !cwd.is_empty() && p.starts_with(&format!("{cwd}/")) => p[cwd.len() + 1..].to_string(),
        _ => p.clone(),
    }
}

/// `input.field ?? fallback`, as text.
fn or_default(input: &Value, field: &str, fallback: &str) -> String {
    match input.get(field) {
        None | Some(Value::Null) => fallback.to_string(),
        Some(v) => js_to_string(v),
    }
}

pub fn label_for_tool_use(block: &Value, cwd: Option<&str>) -> Option<String> {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return None;
    }
    let name = block.get("name").and_then(Value::as_str)?;
    let empty = Value::Object(Default::default());
    let input = block.get("input").filter(|i| i.is_object()).unwrap_or(&empty);
    Some(match name {
        "Read" => clean(&format!("Reading {}", short_path(input.get("file_path"), cwd))),
        "Edit" | "MultiEdit" | "Write" => clean(&format!("Editing {}", short_path(input.get("file_path"), cwd))),
        "NotebookEdit" => clean(&format!("Editing {}", short_path(input.get("notebook_path"), cwd))),
        "Grep" => clean(&format!("Searching for {}", or_default(input, "pattern", "text"))),
        "Glob" => clean(&format!("Finding files {}", or_default(input, "pattern", ""))),
        "Bash" => clean(&format!("Running {}", or_default(input, "command", "a command"))),
        "WebFetch" | "WebSearch" => "Fetching a web page".to_string(),
        other if other.starts_with("mcp__") => {
            let server = other.split("__").nth(1).filter(|s| !s.is_empty()).unwrap_or("a tool");
            clean(&format!("Asking {server}"))
        }
        other => clean(&format!("Using {other}")),
    })
}

/// The label for the latest tool call in an assistant message, or `None`.
pub fn label_for_event(event: &Value, cwd: Option<&str>) -> Option<String> {
    if event.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = event.get("message")?.get("content")?.as_array()?;
    let last = content.iter().rfind(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))?;
    label_for_tool_use(last, cwd)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamResult {
    pub saw_result: bool,
    pub text: String,
    pub session_id: Option<String>,
    pub permission_denials: Vec<String>,
    pub is_error: bool,
}

#[derive(Default)]
pub struct StreamAccumulator {
    cwd: Option<String>,
    init_session: Option<String>,
    result_event: Option<Value>,
}

impl StreamAccumulator {
    pub fn new(cwd: Option<&str>) -> Self {
        StreamAccumulator { cwd: cwd.map(str::to_string), ..Default::default() }
    }

    /// Feeds one line; returns the progress label it produced, if any.
    pub fn feed(&mut self, line: &str) -> Option<String> {
        let event = parse_stream_line(line)?;
        let kind = event.get("type").and_then(Value::as_str);
        if kind == Some("system") && event.get("subtype").and_then(Value::as_str) == Some("init") {
            if let Some(id) = event.get("session_id").and_then(Value::as_str) {
                self.init_session = Some(id.to_string());
            }
        }
        let label = label_for_event(&event, self.cwd.as_deref());
        if kind == Some("result") {
            self.result_event = Some(event);
        }
        label
    }

    pub fn result(&self) -> StreamResult {
        let Some(ev) = &self.result_event else {
            return StreamResult { saw_result: false, text: String::new(), session_id: self.init_session.clone(), permission_denials: vec![], is_error: false };
        };
        let denials = ev
            .get("permission_denials")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|d| d.get("tool_name").and_then(Value::as_str).unwrap_or("?").to_string()).collect())
            .unwrap_or_default();
        StreamResult {
            saw_result: true,
            text: ev.get("result").and_then(Value::as_str).unwrap_or("").to_string(),
            session_id: ev.get("session_id").and_then(Value::as_str).map(str::to_string).or_else(|| self.init_session.clone()),
            permission_denials: denials,
            is_error: ev.get("is_error") == Some(&Value::Bool(true)),
        }
    }
}

/// Lets one call through per `min_ms`; the rest are dropped (a label is only
/// a hint).
pub struct Throttle<N> {
    min_ms: i64,
    now: N,
    last: Option<i64>,
}

impl<N: FnMut() -> i64> Throttle<N> {
    pub fn new(min_ms: i64, now: N) -> Self {
        Throttle { min_ms, now, last: None }
    }

    pub fn call(&mut self, f: impl FnOnce()) {
        let t = (self.now)();
        if self.last.is_none_or(|last| t - last >= self.min_ms) {
            self.last = Some(t);
            f();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn init() -> String {
        json!({ "type": "system", "subtype": "init", "session_id": "s-1", "cwd": "/w", "tools": [] }).to_string()
    }

    fn read_event(file: &str) -> String {
        json!({ "type": "assistant", "message": { "role": "assistant", "content": [
            { "type": "text", "text": "ok" },
            { "type": "tool_use", "id": "t1", "name": "Read", "input": { "file_path": file } } ] }, "session_id": "s-1" })
        .to_string()
    }

    fn result_line() -> String {
        json!({ "type": "result", "subtype": "success", "is_error": false, "result": "All done.", "session_id": "s-1",
                "permission_denials": [{ "tool_name": "Bash", "tool_use_id": "t2", "tool_input": {} }] })
        .to_string()
    }

    #[test]
    fn parse_stream_line_parses_typed_json_objects_and_ignores_everything_else() {
        assert_eq!(parse_stream_line(&init()).unwrap()["subtype"], "init");
        for bad in ["not json", "{\"no\":\"type\"}", "", "{\"type\":5}", "[1]", " {\"type\":\"x\"}"] {
            assert!(parse_stream_line(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn label_for_tool_use_names_the_action_with_paths_relative_to_the_working_directory() {
        let label = |name: &str, input: Value| label_for_tool_use(&json!({ "type": "tool_use", "name": name, "input": input }), Some("/w"));
        let eq = |name: &str, input: Value, want: &str| assert_eq!(label(name, input).as_deref(), Some(want), "{name}");
        eq("Read", json!({ "file_path": "/w/src/a.ts" }), "Reading src/a.ts");
        eq("Read", json!({ "file_path": "/elsewhere/b.ts" }), "Reading /elsewhere/b.ts");
        eq("Edit", json!({ "file_path": "/w/x.js" }), "Editing x.js");
        eq("Grep", json!({ "pattern": "TODO" }), "Searching for TODO");
        eq("Glob", json!({ "pattern": "**/*.ts" }), "Finding files **/*.ts");
        eq("Bash", json!({ "command": "git diff --stat" }), "Running git diff --stat");
        eq("mcp__acme-bitbucket__get_pull_request", json!({}), "Asking acme-bitbucket");
        eq("WebFetch", json!({ "url": "https://secret.example/?t=abc" }), "Fetching a web page");
        eq("SomethingNew", json!({}), "Using SomethingNew");
        eq("Grep", json!({}), "Searching for text");
        eq("Bash", json!({ "command": null }), "Running a command");
        eq("Read", json!({}), "Reading");
        assert_eq!(label_for_tool_use(&json!({ "type": "text" }), Some("/w")), None);
        // A path outside, or equal to, the cwd is kept as is.
        eq("Read", json!({ "file_path": "/w" }), "Reading /w");
        assert_eq!(label_for_tool_use(&json!({ "type": "tool_use", "name": "Read", "input": { "file_path": "/w/a" } }), None).as_deref(), Some("Reading /w/a"));
    }

    #[test]
    fn labels_are_one_line_control_characters_stripped_capped_at_80_characters() {
        let cmd = format!("echo a\nb\u{7}c {}", "x".repeat(200));
        let l = label_for_tool_use(&json!({ "type": "tool_use", "name": "Bash", "input": { "command": cmd } }), Some("/w")).unwrap();
        assert!(!l.contains('\n') && !l.contains('\u{7}'));
        assert!(utf16_len(&l) <= 80);
        assert!(l.ends_with('\u{2026}'));
        assert!(l.starts_with("Running echo a b c xxx"));
    }

    #[test]
    fn the_accumulator_returns_labels_for_tool_calls_and_the_final_result_at_the_end() {
        let mut acc = StreamAccumulator::new(Some("/w"));
        assert_eq!(acc.feed(&init()), None);
        assert_eq!(acc.feed(&read_event("/w/a.ts")).as_deref(), Some("Reading a.ts"));
        assert_eq!(acc.feed("garbage"), None);
        let before = acc.result();
        assert!(!before.saw_result);
        assert_eq!(before.session_id.as_deref(), Some("s-1"), "the init session is kept");
        acc.feed(&result_line());
        assert_eq!(
            acc.result(),
            StreamResult { saw_result: true, text: "All done.".into(), session_id: Some("s-1".into()), permission_denials: vec!["Bash".into()], is_error: false }
        );
    }

    #[test]
    fn a_result_without_a_session_falls_back_to_the_init_one_and_odd_denials_become_question_marks() {
        let mut acc = StreamAccumulator::new(None);
        acc.feed(&init());
        acc.feed(&json!({ "type": "result", "is_error": true, "permission_denials": [{}, 3] }).to_string());
        let r = acc.result();
        assert_eq!((r.session_id.as_deref(), r.is_error, r.text.as_str()), (Some("s-1"), true, ""));
        assert_eq!(r.permission_denials, ["?", "?"]);
    }

    #[test]
    fn the_last_tool_use_in_a_message_wins() {
        let ev = json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": "Read", "input": { "file_path": "/w/a" } },
            { "type": "tool_use", "name": "Grep", "input": { "pattern": "x" } } ] } });
        assert_eq!(label_for_event(&ev, Some("/w")).as_deref(), Some("Searching for x"));
        assert_eq!(label_for_event(&json!({ "type": "user" }), None), None);
        assert_eq!(label_for_event(&json!({ "type": "assistant", "message": { "content": [] } }), None), None);
    }

    #[test]
    fn throttle_lets_one_call_through_per_interval() {
        let t = std::cell::Cell::new(1000_i64);
        let mut throttle = Throttle::new(1000, || t.get());
        let mut seen = vec![];
        throttle.call(|| seen.push("a"));
        throttle.call(|| seen.push("b"));
        t.set(1999);
        throttle.call(|| seen.push("c"));
        t.set(2000);
        throttle.call(|| seen.push("d"));
        assert_eq!(seen, ["a", "d"]);
    }
}
