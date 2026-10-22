//! Which Jira projects/issue types the features work on (port of
//! `core/targets.js`; the Jenkins pipelines live with their feature, in a
//! pack). The defaults come from the profile, so an install with no new config
//! behaves as before. Pure: settings validation uses it, features read it at
//! request time, and the server serves the effective values at `GET /targets`.

use crate::environment;
use crate::js_text::{js_trim, utf16_len};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

const MAX_PROJECTS: usize = 20;
const MAX_ISSUE_TYPES: usize = 10;
const ISSUE_TYPE_MAX: usize = 40;
static PROJECT_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9]{0,19}$").unwrap());

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

pub fn default_projects() -> Vec<String> {
    strings(environment::current().section("targets").and_then(|t| t.get("projects")))
}

/// No type filter by default: any issue of a configured project counts; a list narrows it.
pub fn default_issue_types() -> Vec<String> {
    strings(environment::current().section("targets").and_then(|t| t.get("issueTypes")))
}

fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}')
}

fn dedupe_push(out: &mut Vec<String>, item: String) {
    if !out.contains(&item) {
        out.push(item);
    }
}

pub fn validate_projects(value: &Value) -> Result<Vec<String>, String> {
    let list = value.as_array().filter(|l| !l.is_empty() && l.len() <= MAX_PROJECTS).ok_or_else(|| format!("must be a list of 1 to {MAX_PROJECTS} project keys"))?;
    let mut out = vec![];
    for p in list {
        match p.as_str().map(js_trim).filter(|t| PROJECT_KEY_RE.is_match(t)) {
            Some(t) => dedupe_push(&mut out, t.to_uppercase()),
            None => return Err(format!("\"{}\" isn't a Jira project key", crate::js_text::slice_utf16(&crate::js_text::js_to_string(p), 30))),
        }
    }
    Ok(out)
}

pub fn validate_issue_types(value: &Value) -> Result<Vec<String>, String> {
    let list = value.as_array().filter(|l| l.len() <= MAX_ISSUE_TYPES).ok_or_else(|| format!("must be a list of at most {MAX_ISSUE_TYPES} issue types (empty = any type)"))?;
    let mut out = vec![];
    for t in list {
        let name = t.as_str().map(js_trim).unwrap_or("");
        if name.is_empty() || utf16_len(name) > ISSUE_TYPE_MAX || name.chars().any(is_control) {
            return Err(format!("issue types must be 1 to {ISSUE_TYPE_MAX} characters of plain text"));
        }
        dedupe_push(&mut out, name.to_string());
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeTargets {
    pub projects: Vec<String>,
    pub issue_types: Vec<String>,
}

pub fn analyze_targets_from(config: &Value) -> AnalyzeTargets {
    let a = &config["analyzeIssue"];
    AnalyzeTargets {
        projects: a.get("projects").and_then(|v| validate_projects(v).ok()).unwrap_or_else(default_projects),
        issue_types: a.get("issueTypes").and_then(|v| validate_issue_types(v).ok()).unwrap_or_else(default_issue_types),
    }
}

/// "PROJ tickets", "CI/PROJ Bugs/Tasks": for messages.
pub fn describe_analyze_scope(t: &AnalyzeTargets) -> String {
    let projects = if t.projects.is_empty() { String::new() } else { format!("{} ", t.projects.join("/")) };
    let types = if t.issue_types.is_empty() { "tickets".to_string() } else { t.issue_types.iter().map(|x| format!("{x}s")).collect::<Vec<_>>().join("/") };
    format!("{projects}{types}")
}

const MAX_COMPONENTS: usize = 50;
static REPO_REF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());

/// Jira component name to the "PROJECT/repo" key of a mapped clone (`config.repos`).
pub fn validate_component_repo_map(value: &Value) -> Result<Map<String, Value>, String> {
    let entries = value.as_object().ok_or("must be a list of `component = PROJECT/repo` lines")?;
    if entries.len() > MAX_COMPONENTS {
        return Err(format!("can have at most {MAX_COMPONENTS} components"));
    }
    let mut out = Map::new();
    for (component, repo) in entries {
        let name = js_trim(component);
        if name.is_empty() || utf16_len(name) > 60 || name.chars().any(is_control) {
            return Err("a component name must be 1 to 60 characters of plain text".into());
        }
        match repo.as_str().filter(|r| REPO_REF_RE.is_match(r) && !r.contains("..")) {
            Some(r) => out.insert(name.to_string(), json!(r)),
            None => return Err(format!("\"{}\" isn't a PROJECT/repo", crate::js_text::slice_utf16(&crate::js_text::js_to_string(repo), 40))),
        };
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn validate_projects_upper_cases_and_de_duplicates_project_keys_and_rejects_junk() {
        assert_eq!(validate_projects(&json!(["proj", "CI", "PROJ"])), Ok(s(&["PROJ", "CI"])));
        let too_many: Vec<String> = (0..21).map(|i| format!("P{i}")).collect();
        for bad in [json!([]), json!("PROJ"), json!(["PROJ-1"]), json!(["1PROJ"]), json!(["a".repeat(21)]), json!(too_many), json!([5])] {
            assert!(validate_projects(&bad).is_err(), "{bad}");
        }
        assert_eq!(validate_projects(&json!(["a-b"])).unwrap_err(), "\"a-b\" isn't a Jira project key");
        assert_eq!(validate_projects(&json!([" ci "])), Ok(s(&["CI"])));
    }

    #[test]
    fn validate_issue_types_trims_and_de_duplicates_type_names_and_rejects_junk() {
        assert_eq!(validate_issue_types(&json!([" Bug ", "Task", "Bug"])), Ok(s(&["Bug", "Task"])));
        assert_eq!(validate_issue_types(&json!([])), Ok(vec![])); // empty = any type
        for bad in [json!("Bug"), json!([""]), json!(["a\nb"]), json!(["x".repeat(41)]), json!([1]), json!(vec!["t"; 11])] {
            assert!(validate_issue_types(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn analyze_targets_from_reads_valid_lists_and_describe_analyze_scope_names_them() {
        let t = analyze_targets_from(&json!({ "analyzeIssue": { "projects": ["ci", "proj"], "issueTypes": ["Bug", "Task"] } }));
        assert_eq!(t, AnalyzeTargets { projects: s(&["CI", "PROJ"]), issue_types: s(&["Bug", "Task"]) });
        assert_eq!(describe_analyze_scope(&t), "CI/PROJ Bugs/Tasks");
    }

    #[test]
    fn invalid_or_missing_lists_fall_back_to_the_profile_defaults() {
        let d = analyze_targets_from(&json!({}));
        assert_eq!(d, AnalyzeTargets { projects: default_projects(), issue_types: default_issue_types() });
        assert_eq!(d.projects, ["PROJ"], "the placeholder profile's target");
        assert_eq!(analyze_targets_from(&json!({ "analyzeIssue": { "projects": [], "issueTypes": "x" } })), d);
    }

    #[test]
    fn an_empty_issue_type_list_means_any_type_and_a_list_still_narrows_it() {
        assert!(analyze_targets_from(&json!({ "analyzeIssue": { "issueTypes": [] } })).issue_types.is_empty());
        assert_eq!(analyze_targets_from(&json!({ "analyzeIssue": { "issueTypes": ["Customer Issue"] } })).issue_types, ["Customer Issue"]);
        assert_eq!(describe_analyze_scope(&AnalyzeTargets { projects: s(&["OPS"]), issue_types: vec![] }), "OPS tickets");
    }

    #[test]
    fn validate_component_repo_map_accepts_component_to_project_repo_and_rejects_the_rest() {
        assert_eq!(
            Value::Object(validate_component_repo_map(&json!({ "UI": "ACME/sample-app", " API ": "ACME/sample-service" })).unwrap()),
            json!({ "UI": "ACME/sample-app", "API": "ACME/sample-service" })
        );
        assert_eq!(validate_component_repo_map(&json!({})), Ok(Map::new()));
        let many: Map<String, Value> = (0..51).map(|i| (format!("c{i}"), json!("A/b"))).collect();
        for bad in [json!([]), json!("x"), Value::Null, json!({ "UI": "sample-app" }), json!({ "UI": "ACME/sample-app/extra" }), json!({ "UI": "ACME/../x" }), json!({ "": "ACME/sample-app" }), json!({ "UI": 5 }), json!({ "a\nb": "ACME/sample-app" }), Value::Object(many)] {
            assert!(validate_component_repo_map(&bad).is_err(), "{bad}");
        }
    }
}
