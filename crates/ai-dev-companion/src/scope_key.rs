//! A `scopeKey` ties a job to the browser page it belongs to (a Bitbucket PR,
//! a Jira issue, a pre-deployment-stats Jenkins pipeline); port of
//! `core/scope-key.js`. The extension reads it back via `GET /jobs/lookup` so a
//! page can show "there's already a pending job for this PR/issue" instead of
//! a duplicate. `lookup_job` works on plain job JSON.

use crate::js_number::{js_number, json_number};
use crate::js_text::{js_to_string, utf16_len};
use crate::packs::{PackRegistry, ScopeKeyKind};
use crate::prereqs::{is_safe_repo_segment, is_safe_repo_segment_value, GitRemoteRules};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// A positive integer, given either as a real number or as a plain digit
/// string: the two shapes an MCP tool's `prId` argument or an extension's JSON
/// body might send it as. `0`, negatives, non-integers ("1.5") and a string
/// with anything but digits ("12a") are all rejected.
fn is_positive_int(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f.fract() == 0.0 && f > 0.0),
        Some(Value::String(s)) => !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s.bytes().any(|b| b != b'0'),
        _ => false,
    }
}

/// `<host>:<project>/<repo>#<n>` for resolve-conflict and
/// address-review-comments, whose payload is `{project, repo, prId}`:
/// `bitbucket:PROJECT/repo#n` (project upper-cased, repo lower-cased, as
/// Bitbucket Server itself cases them) or `github:owner/repo#n` (both
/// lower-cased), by this distribution's git provider. `None` for anything that
/// doesn't look like a real project/repo/prId.
pub fn pr_scope_key(payload: &Value, git: GitRemoteRules) -> Option<String> {
    if !payload.is_object() {
        return None;
    }
    let (project, repo, pr_id) = (payload.get("project"), payload.get("repo"), payload.get("prId"));
    if !is_safe_repo_segment_value(project) || !is_safe_repo_segment_value(repo) || !is_positive_int(pr_id) {
        return None;
    }
    let id = js_to_string(&json_number(js_number(pr_id)));
    Some(git.pr_key(project?.as_str()?, repo?.as_str()?, &id))
}

// Jira issue keys are `<PROJECT KEY><ticket number>`, e.g. "PROJ-1": no
// internal dashes in the project part, so this doesn't reuse
// is_safe_repo_segment (which would also accept things like "proj.1").
static ISSUE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z][A-Za-z0-9]*)-([1-9][0-9]*)$").unwrap());

/// `jira:KEY-n` for analyze-issue, whose payload is `{issueKey}`, upper-cased
/// to match how the rest of the service treats issue keys. `None` for a payload
/// with no ticket number ("PROJ") or anything else malformed.
pub fn jira_scope_key(payload: &Value) -> Option<String> {
    let m = ISSUE_KEY.captures(payload.get("issueKey")?.as_str()?)?;
    Some(format!("jira:{}-{}", m[1].to_uppercase(), &m[2]))
}

/// The scopeKey a job for `feature_id` belongs under, given the payload its
/// start call was made with, or `None` if the feature doesn't tag jobs this
/// way or the payload doesn't look like a real one for it. Each feature
/// declares its own rule in its pack.
pub fn scope_key_for<F>(registry: &PackRegistry<F>, feature_id: &str, payload: &Value, git: GitRemoteRules) -> Option<String> {
    registry.scope_key_for(feature_id, payload, &|kind, p| match kind {
        ScopeKeyKind::Pr | ScopeKeyKind::BitbucketPr => pr_scope_key(p, git),
        ScopeKeyKind::JiraIssue => jira_scope_key(p),
    })
}

fn is_positive_int_string(v: &str) -> bool {
    let b = v.as_bytes();
    !b.is_empty() && (b'1'..=b'9').contains(&b[0]) && b.iter().all(u8::is_ascii_digit)
}

const MAX_SCOPE_KEY_LENGTH: usize = 300;

static HELM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^jenkins:([^#]+)$").unwrap());
static PR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:bitbucket|github):([^/]+)/([^#]+)#([^#]+)$").unwrap());
static JIRA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^jira:([^-]+)-([^-]+)$").unwrap());
static BUILD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^jenkins:([^#]+)#([^#]+)$").unwrap());

/// Whether `s` is a well-formed scopeKey, checked with anchored patterns so
/// nothing but exactly one of the shapes below can pass, however it's spelled:
/// `bitbucket:<SEG>/<seg>#<n>` or `github:<seg>/<seg>#<n>`, `jira:<KEY>-<n>`,
/// `jenkins:<helm job>`, or `jenkins:<seg>#<n>`. Every path-like segment is
/// also checked with `is_safe_repo_segment`, every `n` must be a positive
/// integer, and a trailing newline or anything over 300 characters is refused
/// (`GET /jobs/lookup`'s query string has no other length limit).
pub fn is_valid_scope_key(s: &str) -> bool {
    if utf16_len(s) > MAX_SCOPE_KEY_LENGTH {
        return false;
    }
    if let Some(m) = HELM.captures(s) {
        return is_safe_repo_segment(&m[1]);
    }
    if let Some(m) = PR.captures(s) {
        return is_safe_repo_segment(&m[1]) && is_safe_repo_segment(&m[2]) && is_positive_int_string(&m[3]);
    }
    if let Some(m) = JIRA.captures(s) {
        return is_safe_repo_segment(&m[1]) && is_positive_int_string(&m[2]);
    }
    if let Some(m) = BUILD.captures(s) {
        return is_safe_repo_segment(&m[1]) && is_positive_int_string(&m[2]);
    }
    false
}

/// The newest (by `createdAt`) job that belongs to `scope_key` and
/// `feature_id`, was started through an MCP tool or a background watcher (an
/// extension-started job never needs looking up this way, since the page that
/// started it already has its id), and hasn't already been resolved
/// (`approved`/`rejected`), or `None`.
pub fn lookup_job<'a>(jobs: &'a [Value], scope_key: &str, feature_id: &str) -> Option<&'a Value> {
    let mut newest: Option<&Value> = None;
    for job in jobs {
        if job["scopeKey"] != scope_key || job["featureId"] != feature_id {
            continue;
        }
        if !matches!(job["startedVia"].as_str(), Some("mcp" | "watcher")) || matches!(job["status"].as_str(), Some("approved" | "rejected")) {
            continue;
        }
        let newer = newest.is_none_or(|n| job["createdAt"].as_f64().zip(n["createdAt"].as_f64()).is_some_and(|(a, b)| a > b));
        if newer {
            newest = Some(job);
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::{FeatureDescriptor, FeatureSpec, Pack, ScopeKey};
    use GitRemoteRules::{BitbucketDc, Github};
    use serde_json::json;

    fn registry() -> PackRegistry<()> {
        let spec = |id: &str, kind: ScopeKeyKind| {
            let mut s = FeatureSpec::new(FeatureDescriptor::new(id, id), ());
            s.scope_key = ScopeKey::Kind(kind);
            s
        };
        let none = FeatureSpec::new(FeatureDescriptor::new("plain", "plain"), ());
        PackRegistry::new(vec![Pack::new(
            "t",
            vec![
                spec("resolve-conflict", ScopeKeyKind::Pr),
                spec("address-review-comments", ScopeKeyKind::BitbucketPr),
                spec("analyze-issue", ScopeKeyKind::JiraIssue),
                spec("ticket-workspace", ScopeKeyKind::JiraIssue),
                spec("ticket-to-pr", ScopeKeyKind::JiraIssue),
                none,
            ],
        )])
        .unwrap()
    }

    fn key(feature: &str, payload: Value, git: GitRemoteRules) -> Option<String> {
        scope_key_for(&registry(), feature, &payload, git)
    }

    #[test]
    fn pr_keys_follow_the_git_host_and_case_each_part_like_that_host() {
        assert_eq!(key("resolve-conflict", json!({ "project": "ci", "repo": "Sample-App", "prId": 12 }), BitbucketDc).as_deref(), Some("bitbucket:CI/sample-app#12"));
        assert_eq!(key("resolve-conflict", json!({ "project": "Octo", "repo": "Hello", "prId": 12 }), Github).as_deref(), Some("github:octo/hello#12"));
        assert_eq!(key("address-review-comments", json!({ "project": "ACME", "repo": "sample-app", "prId": 7 }), BitbucketDc).as_deref(), Some("bitbucket:ACME/sample-app#7"));
        assert_eq!(key("address-review-comments", json!({ "project": "acme", "repo": "Sample-App", "prId": "7" }), Github).as_deref(), Some("github:acme/sample-app#7"));
        assert_eq!(key("resolve-conflict", json!({ "project": "CI", "repo": "sample-app", "prId": "12" }), Github).as_deref(), Some("github:ci/sample-app#12"));
    }

    #[test]
    fn a_bad_pr_payload_gives_none() {
        for bad in [json!(0), json!(-1), json!("12a"), json!(1.5), json!("0"), json!(""), Value::Null, json!(true)] {
            assert_eq!(key("resolve-conflict", json!({ "project": "CI", "repo": "sample-app", "prId": bad }), Github), None, "{bad}");
        }
        for project in ["CI/x", "..", "", "-x"] {
            assert_eq!(key("resolve-conflict", json!({ "project": project, "repo": "sample-app", "prId": 12 }), Github), None, "{project}");
        }
        assert_eq!(key("resolve-conflict", Value::Null, Github), None);
        assert_eq!(key("resolve-conflict", json!({}), Github), None);
        assert_eq!(key("resolve-conflict", json!({ "project": 5, "repo": "r", "prId": 1 }), Github), None);
        assert_eq!(key("resolve-conflict", json!({ "project": "o", "repo": "r", "prId": 7.0 }), Github).as_deref(), Some("github:o/r#7"));
    }

    #[test]
    fn jira_keys_are_upper_cased_and_need_a_ticket_number() {
        assert_eq!(key("analyze-issue", json!({ "issueKey": "proj-1" }), Github).as_deref(), Some("jira:PROJ-1"));
        assert_eq!(key("analyze-issue", json!({ "issueKey": "PROJ" }), Github), None);
        assert_eq!(key("analyze-issue", json!({}), Github), None);
        assert_eq!(key("analyze-issue", json!({ "issueKey": "PROJ-0" }), Github), None);
        assert_eq!(key("ticket-workspace", json!({ "issueKey": "proj-7" }), Github).as_deref(), Some("jira:PROJ-7"));
        assert_eq!(key("ticket-to-pr", json!({ "issueKey": "PROJ-7", "adopt": true }), Github).as_deref(), Some("jira:PROJ-7"));
        assert_eq!(key("ticket-to-pr", json!({ "issueKey": "PROJ" }), Github), None);
    }

    #[test]
    fn an_unknown_feature_or_one_without_a_rule_gives_none() {
        assert_eq!(key("some-other-feature", json!({ "project": "CI", "repo": "sample-app", "prId": 12 }), Github), None);
        assert_eq!(key("plain", json!({ "issueKey": "PROJ-1" }), Github), None);
    }

    #[test]
    fn is_valid_scope_key_accepts_exactly_the_shapes_the_service_produces() {
        for ok in ["bitbucket:CI/sample-app#12", "jira:PROJ-1", "jenkins:helm_main", "jenkins:helm_main#42", "github:octo/hello#42", "github:octo/.github#1", "jenkins:helm_other"] {
            assert!(is_valid_scope_key(ok), "{ok}");
        }
    }

    #[test]
    fn is_valid_scope_key_rejects_malformed_keys() {
        let long = format!("bitbucket:CI/sample-app#{}", "1".repeat(301));
        for bad in [
            "bitbucket:CI/sample-app#12\n",
            "jira:PROJ-1 OR 1=1",
            long.as_str(),
            "jenkins:other job",
            "jenkins:../other_job",
            "jenkins:bad job",
            "jenkins:",
            "github:octo/hello#0",
            "github:octo/hello#4x",
            "github:../x/hello#1",
            "github:octo/hello#1\n",
            "gitlab:octo/hello#1",
            "jira:PROJ-01",
            "jenkins:job#0",
            "",
        ] {
            assert!(!is_valid_scope_key(bad), "{bad:?}");
        }
    }

    fn job(over: Value) -> Value {
        let mut j = json!({ "id": "id", "featureId": "resolve-conflict", "status": "running", "data": {}, "scopeKey": "bitbucket:CI/sample-app#12", "startedVia": "mcp", "createdAt": 0, "updatedAt": 0 });
        for (k, v) in over.as_object().unwrap() {
            j[k] = v.clone();
        }
        j
    }

    fn found(jobs: &[Value], scope: &str, feature: &str) -> Option<String> {
        lookup_job(jobs, scope, feature).map(|j| j["id"].as_str().unwrap().to_string())
    }

    #[test]
    fn lookup_job_picks_the_newest_matching_mcp_or_watcher_job_that_is_still_open() {
        let s = "bitbucket:CI/sample-app#12";
        assert_eq!(found(&[job(json!({ "id": "older", "createdAt": 1 })), job(json!({ "id": "newer", "createdAt": 2 }))], s, "resolve-conflict").as_deref(), Some("newer"));
        assert_eq!(found(&[job(json!({ "id": "newer", "createdAt": 2 })), job(json!({ "id": "older", "createdAt": 1 }))], s, "resolve-conflict").as_deref(), Some("newer"));
        let ext = job(json!({ "id": "extension", "startedVia": "extension", "createdAt": 5 }));
        assert_eq!(found(&[ext, job(json!({ "id": "mcp", "createdAt": 1 }))], s, "resolve-conflict").as_deref(), Some("mcp"));
        assert_eq!(found(&[job(json!({ "id": "w", "startedVia": "watcher" }))], s, "resolve-conflict").as_deref(), Some("w"));
        let done = [job(json!({ "id": "approved", "status": "approved", "createdAt": 5 })), job(json!({ "id": "rejected", "status": "rejected", "createdAt": 4 })), job(json!({ "id": "pending", "status": "pending-start", "createdAt": 1 }))];
        assert_eq!(found(&done, s, "resolve-conflict").as_deref(), Some("pending"));
        let other = [job(json!({ "id": "other", "featureId": "address-review-comments", "createdAt": 5 })), job(json!({ "id": "this", "createdAt": 1 }))];
        assert_eq!(found(&other, s, "resolve-conflict").as_deref(), Some("this"));
        assert_eq!(found(&[job(json!({ "id": "a" }))], "jira:PROJ-1", "analyze-issue"), None);
        assert_eq!(found(&[], s, "resolve-conflict"), None);
    }
}
