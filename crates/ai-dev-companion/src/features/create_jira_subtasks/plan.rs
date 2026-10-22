//! Pure logic for create-jira-subtasks (port of
//! `features/create-jira-subtasks/plan.js`). The feature is a generic "add up
//! to 10 subtasks, each with its own name and assignee" tool: the extension's
//! form collects the rows directly, so there's no fixed ladder or
//! default-assignee mapping to encode here.

use crate::js_text::{js_to_string, js_trim, js_truthy};
use serde_json::Value;

pub const MAX_SUBTASK_ROWS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueKey {
    pub key: String,
    pub project_key: String,
}

/// `^([A-Z][A-Z0-9_]*)-(\d+)$` on the already upper-cased text.
pub(crate) fn split_issue_key(key: &str) -> Option<&str> {
    let (project, number) = key.split_once('-')?;
    let b = project.as_bytes();
    let project_ok = !b.is_empty() && b[0].is_ascii_uppercase() && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_');
    // `[A-Z0-9_]*` can't contain "-", so the first "-" is the separator.
    (project_ok && !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit())).then_some(project)
}

/// Upper-cases and validates an issue key, rejecting malformed input instead of
/// silently producing a bad project key. `raw` is whatever the client sent.
pub fn parse_issue_key_value(raw: Option<&Value>) -> Result<IssueKey, String> {
    let text = if js_truthy(raw) { js_to_string(raw.unwrap()) } else { String::new() };
    let key = js_trim(&text).to_uppercase();
    match split_issue_key(&key) {
        Some(project) => Ok(IssueKey { project_key: project.to_string(), key }),
        None => {
            let shown = raw.map_or_else(|| "undefined".to_string(), js_to_string);
            Err(format!("\"{shown}\" doesn't look like a Jira issue key (expected e.g. \"PROJ-34541\")."))
        }
    }
}

pub fn parse_issue_key(raw: &str) -> Result<IssueKey, String> {
    parse_issue_key_value(Some(&Value::String(raw.to_string())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtaskRow {
    pub summary: String,
    pub assignee: String,
}

/// Validates the extension panel's submitted rows: `body.subtasks` must be an
/// array of 1-10 entries, each a non-blank `{ summary, assignee }`. This is the
/// server's own belt-and-suspenders check (the form does its own friendlier
/// validation first). Returns the validated, trimmed rows.
pub fn assert_subtask_rows(body: &Value) -> Result<Vec<SubtaskRow>, String> {
    let rows = match body.get("subtasks").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => a,
        _ => return Err("At least one subtask is required.".into()),
    };
    if rows.len() > MAX_SUBTASK_ROWS {
        return Err(format!("At most {MAX_SUBTASK_ROWS} subtasks can be created at once."));
    }
    let field = |row: &Value, k: &str| match row.get(k) {
        v if js_truthy(v) => js_trim(&js_to_string(v.unwrap())).to_string(),
        _ => String::new(),
    };
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let (summary, assignee) = (field(row, "summary"), field(row, "assignee"));
            if summary.is_empty() {
                return Err(format!("Subtask #{} needs a name.", i + 1));
            }
            if assignee.is_empty() {
                return Err(format!("Subtask #{} (\"{summary}\") needs an assignee.", i + 1));
            }
            Ok(SubtaskRow { summary, assignee })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_issue_key_upper_cases_and_splits_the_project_key() {
        let r = parse_issue_key("proj-34541").unwrap();
        assert_eq!((r.key.as_str(), r.project_key.as_str()), ("PROJ-34541", "PROJ"));
        let r = parse_issue_key("CIS2_X-71290").unwrap();
        assert_eq!((r.key.as_str(), r.project_key.as_str()), ("CIS2_X-71290", "CIS2_X"));
        assert_eq!(parse_issue_key("  proj-1  ").unwrap().key, "PROJ-1");
    }

    #[test]
    fn parse_issue_key_rejects_malformed_input() {
        for bad in ["not-an-issue-key", "", "PROJ-", "-1", "PROJ-1-2", "1PROJ-1", "PROJ-1x", "PRO J-1", "PROJ_-1x"] {
            let err = parse_issue_key(bad).unwrap_err();
            assert!(err.contains("doesn't look like a Jira issue key"), "{bad}");
        }
        assert_eq!(parse_issue_key("nope").unwrap_err(), "\"nope\" doesn't look like a Jira issue key (expected e.g. \"PROJ-34541\").");
        assert_eq!(parse_issue_key_value(None).unwrap_err(), "\"undefined\" doesn't look like a Jira issue key (expected e.g. \"PROJ-34541\").");
        assert!(parse_issue_key_value(Some(&json!(null))).unwrap_err().starts_with("\"null\""));
        assert!(parse_issue_key_value(Some(&json!(12))).is_err());
        assert_eq!(parse_issue_key_value(Some(&json!("ab-3"))).unwrap().project_key, "AB");
    }

    fn rows(v: Value) -> Value {
        json!({ "subtasks": v })
    }

    #[test]
    fn subtask_rows_accept_a_well_formed_row_exactly_the_max_and_trim_whitespace() {
        let r = assert_subtask_rows(&rows(json!([{ "summary": "Implement", "assignee": "jdoe" }]))).unwrap();
        assert_eq!(r, [SubtaskRow { summary: "Implement".into(), assignee: "jdoe".into() }]);
        let max: Vec<Value> = (0..MAX_SUBTASK_ROWS).map(|i| json!({ "summary": format!("Task {i}"), "assignee": "jdoe" })).collect();
        assert_eq!(assert_subtask_rows(&rows(Value::Array(max))).unwrap().len(), MAX_SUBTASK_ROWS);
        let r = assert_subtask_rows(&rows(json!([{ "summary": "  Implement  ", "assignee": "  jdoe  " }]))).unwrap();
        assert_eq!(r, [SubtaskRow { summary: "Implement".into(), assignee: "jdoe".into() }]);
    }

    #[test]
    fn subtask_rows_reject_zero_rows_a_missing_array_and_too_many() {
        assert_eq!(assert_subtask_rows(&rows(json!([]))).unwrap_err(), "At least one subtask is required.");
        assert_eq!(assert_subtask_rows(&json!({})).unwrap_err(), "At least one subtask is required.");
        assert_eq!(assert_subtask_rows(&json!(null)).unwrap_err(), "At least one subtask is required.");
        assert_eq!(assert_subtask_rows(&rows(json!("x"))).unwrap_err(), "At least one subtask is required.");
        let many: Vec<Value> = (0..=MAX_SUBTASK_ROWS).map(|i| json!({ "summary": format!("Task {i}"), "assignee": "jdoe" })).collect();
        assert_eq!(assert_subtask_rows(&rows(Value::Array(many))).unwrap_err(), "At most 10 subtasks can be created at once.");
    }

    #[test]
    fn subtask_rows_reject_a_blank_summary_or_assignee_naming_the_row() {
        assert_eq!(assert_subtask_rows(&rows(json!([{ "summary": "  ", "assignee": "jdoe" }]))).unwrap_err(), "Subtask #1 needs a name.");
        assert_eq!(assert_subtask_rows(&rows(json!([{ "summary": "Implement", "assignee": "" }]))).unwrap_err(), "Subtask #1 (\"Implement\") needs an assignee.");
        let err = assert_subtask_rows(&rows(json!([{ "summary": "a", "assignee": "b" }, null]))).unwrap_err();
        assert_eq!(err, "Subtask #2 needs a name.");
        assert_eq!(assert_subtask_rows(&rows(json!(["x"]))).unwrap_err(), "Subtask #1 needs a name.");
        // Non-string values are coerced like `String(x || "")`.
        let r = assert_subtask_rows(&rows(json!([{ "summary": 7, "assignee": 8 }]))).unwrap();
        assert_eq!(r[0], SubtaskRow { summary: "7".into(), assignee: "8".into() });
    }
}
