//! Pure pieces of the Jira calls Ticket to PR makes after opening a PR (port
//! of `core/jira-workflow.js`): read a ticket's basics, pick the review
//! transition by name, and build the remote-link body. The Jira client does
//! the HTTP (Jira Server/DC, API v2).

use crate::app_identity::app_slug;
use crate::js_text::{js_trim, slice_utf16};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;

fn s(v: &Value) -> Value {
    if v.is_string() { v.clone() } else { Value::Null }
}

fn name_of(v: &Value) -> Value {
    if v.is_object() { s(&v["name"]) } else { Value::Null }
}

/// `GET /rest/api/2/issue/{key}?fields=summary,issuetype,status` becomes
/// `{key, summary, issueType, status}`, or `None` for a non-object.
pub fn normalize_issue_basics(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let none = json!({});
    let fields = if raw["fields"].is_object() { &raw["fields"] } else { &none };
    Some(json!({ "key": s(&raw["key"]), "summary": s(&fields["summary"]), "issueType": name_of(&fields["issuetype"]), "status": name_of(&fields["status"]) }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub id: String,
    pub name: Option<String>,
    pub to: Option<String>,
}

/// `GET .../transitions` becomes `[{id, name, to}]` (entries without an id dropped).
pub fn normalize_transitions(raw: &Value) -> Vec<Transition> {
    let list = raw["transitions"].as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter(|t| t.is_object() && (t["id"].is_string() || t["id"].is_number()))
        .map(|t| Transition {
            id: crate::js_text::js_to_string(&t["id"]),
            name: t["name"].as_str().map(String::from),
            to: if t["to"].is_object() { t["to"]["name"].as_str().map(String::from) } else { None },
        })
        .collect()
}

fn fold(v: Option<&str>) -> String {
    v.map(|s| js_trim(s).to_lowercase()).unwrap_or_default()
}

/// The transition whose name, or failing that whose target status, matches
/// `wanted` (case- and space-insensitive), or `None`.
pub fn pick_transition<'a>(transitions: &'a [Transition], wanted: &str) -> Option<&'a Transition> {
    let want = fold(Some(wanted));
    if want.is_empty() {
        return None;
    }
    transitions.iter().find(|t| fold(t.name.as_deref()) == want).or_else(|| transitions.iter().find(|t| fold(t.to.as_deref()) == want))
}

const DESCRIPTION_MAX: usize = 2000;
static ISSUE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]*-[0-9]+$").unwrap());

/// `GET /rest/api/2/search` becomes `[{key, summary, status, statusCategory,
/// issueType, priority, updated, description, url}]` (no key: dropped).
pub fn normalize_search_results(raw: &Value, base_url: Option<&str>) -> Vec<Value> {
    let base = base_url.map(|b| b.trim_end_matches('/')).unwrap_or("");
    let issues = raw["issues"].as_array().map(Vec::as_slice).unwrap_or_default();
    issues
        .iter()
        .filter(|i| i["key"].as_str().is_some_and(|k| ISSUE_KEY.is_match(k)))
        .map(|i| {
            let none = json!({});
            let f = if i["fields"].is_object() { &i["fields"] } else { &none };
            let category = if f["status"].is_object() && f["status"]["statusCategory"].is_object() { s(&f["status"]["statusCategory"]["key"]) } else { Value::Null };
            let key = i["key"].as_str().unwrap_or("");
            json!({
                "key": key,
                "summary": s(&f["summary"]),
                "status": name_of(&f["status"]),
                "statusCategory": category,
                "issueType": name_of(&f["issuetype"]),
                "priority": name_of(&f["priority"]),
                "updated": s(&f["updated"]),
                "description": f["description"].as_str().map_or(Value::Null, |d| json!(slice_utf16(d, DESCRIPTION_MAX))),
                "url": if base.is_empty() { Value::Null } else { json!(format!("{base}/browse/{key}")) },
            })
        })
        .collect()
}

/// The remote-link body that shows a PR on the ticket. `globalId` makes a
/// retry update the same link instead of adding a second one.
pub fn remote_link_body(url: Option<&str>, title: Option<&str>) -> Result<Value, String> {
    let Some(url) = url.filter(|u| u.starts_with("https://")) else { return Err("A remote link needs an https URL.".into()) };
    let text = title.map(js_trim).filter(|t| !t.is_empty()).unwrap_or(url);
    Ok(json!({
        "globalId": format!("{}:pr:{url}", app_slug()),
        "application": { "type": "com.atlassian.bitbucket", "name": "Bitbucket" },
        "relationship": "Pull request",
        "object": { "url": url, "title": slice_utf16(text, 255) },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, name: Option<&str>, to: Option<&str>) -> Transition {
        Transition { id: id.into(), name: name.map(String::from), to: to.map(String::from) }
    }

    #[test]
    fn normalize_issue_basics_reads_key_summary_type_and_status_defensively() {
        assert_eq!(
            normalize_issue_basics(&json!({ "key": "PROJ-7", "fields": { "summary": "Login fails", "issuetype": { "name": "Bug" }, "status": { "name": "Open" } } })),
            Some(json!({ "key": "PROJ-7", "summary": "Login fails", "issueType": "Bug", "status": "Open" }))
        );
        assert_eq!(normalize_issue_basics(&json!({})), Some(json!({ "key": null, "summary": null, "issueType": null, "status": null })));
        assert_eq!(normalize_issue_basics(&json!("x")), None);
        assert_eq!(normalize_issue_basics(&json!([1])), None);
    }

    #[test]
    fn normalize_transitions_keeps_entries_with_an_id_and_stringifies_it() {
        let raw = json!({ "transitions": [{ "id": "21", "name": "Start Review", "to": { "name": "In Review" } }, { "id": 31, "name": "Done" }, { "name": "no id" }] });
        assert_eq!(normalize_transitions(&raw), [t("21", Some("Start Review"), Some("In Review")), t("31", Some("Done"), None)]);
        assert!(normalize_transitions(&Value::Null).is_empty());
    }

    #[test]
    fn pick_transition_matches_the_name_first_then_the_target_status_ignoring_case_and_spaces() {
        let ts = [t("1", Some("In Review"), Some("Review")), t("2", Some("Send to review"), Some("In Review"))];
        assert_eq!(pick_transition(&ts, "in review").unwrap().id, "1");
        assert_eq!(pick_transition(&ts[1..], " In Review ").unwrap().id, "2");
        assert_eq!(pick_transition(&ts, "Done"), None);
        assert_eq!(pick_transition(&ts, ""), None);
    }

    #[test]
    fn remote_link_body_builds_an_idempotent_link_and_refuses_non_https_urls() {
        let url = "https://bb.example/projects/ACME/repos/sample-app/pull-requests/12";
        let body = remote_link_body(Some(url), Some("PR #12: fix")).unwrap();
        assert_eq!(body["globalId"], format!("{}:pr:{url}", app_slug()));
        assert_eq!(body["object"], json!({ "url": url, "title": "PR #12: fix" }));
        assert_eq!(body["application"], json!({ "type": "com.atlassian.bitbucket", "name": "Bitbucket" }));
        assert_eq!(body["relationship"], "Pull request");
        assert_eq!(slice_utf16(remote_link_body(Some("https://x/1"), Some(&"t".repeat(400))).unwrap()["object"]["title"].as_str().unwrap(), 300).len(), 255);
        for bad in [Some("http://bb.example/pr/1"), Some("ftp://x/y"), Some("//bb.example/x"), Some(""), None] {
            assert!(remote_link_body(bad, Some("t")).unwrap_err().contains("https URL"), "{bad:?}");
        }
        for title in [Some(""), Some("   "), None] {
            assert_eq!(remote_link_body(Some(url), title).unwrap()["object"]["title"], url);
        }
        assert_eq!(remote_link_body(Some(url), Some("  PR #1  ")).unwrap()["object"]["title"], "PR #1");
    }

    #[test]
    fn normalize_search_results_keeps_well_formed_keys_and_builds_browse_urls() {
        let raw = json!({ "issues": [
            { "key": "PROJ-7", "fields": { "summary": "S", "status": { "name": "Open", "statusCategory": { "key": "new" } }, "issuetype": { "name": "Bug" }, "priority": { "name": "High" }, "updated": "2026-09-28T10:00:00.000+0000", "description": "d".repeat(3000) } },
            { "key": "proj-8" }, { "summary": "no key" }, "junk",
        ] });
        let out = normalize_search_results(&raw, Some("https://jira.example.com//"));
        assert_eq!(out.len(), 1);
        let i = &out[0];
        assert_eq!((i["key"].as_str(), i["status"].as_str(), i["statusCategory"].as_str(), i["issueType"].as_str(), i["priority"].as_str()), (Some("PROJ-7"), Some("Open"), Some("new"), Some("Bug"), Some("High")));
        assert_eq!(i["description"].as_str().unwrap().len(), 2000);
        assert_eq!(i["url"], "https://jira.example.com/browse/PROJ-7");
        assert_eq!(normalize_search_results(&raw, None)[0]["url"], Value::Null);
        assert!(normalize_search_results(&Value::Null, None).is_empty());
    }
}
