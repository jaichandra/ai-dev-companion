//! The Bitbucket Server/DC paths the ticket features call, and the fallback
//! rule for endpoints that moved between versions (port of
//! `core/bitbucket-endpoints.js`). Pure: the Bitbucket client does the HTTP.
//! Pinned to /rest/api/1.0 (never /latest); plugin endpoints (build-status,
//! default-reviewers) keep their own versioned roots. Every project/repo
//! segment and branch is checked here, so nothing untrusted is ever spliced
//! into a path.

use crate::js_text::slice_utf16;
use crate::prereqs::is_safe_repo_segment;
use crate::uri::encode_uri_component;
use regex::Regex;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::LazyLock;
use url::form_urlencoded;

pub const API_ROOT: &str = "/rest/api/1.0";
static SHA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{7,40}$").unwrap());
// A plain branch name: no leading "-", no "..", no "//", no trailing "/" or
// "." or ".lock", no spaces, quotes or shell/ref metacharacters.
static BRANCH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,199}$").unwrap());
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

pub fn is_safe_branch_name(name: &str) -> bool {
    BRANCH_RE.is_match(name) && !name.contains("..") && !name.contains("//") && !name.ends_with('/') && !name.ends_with('.') && !name.ends_with(".lock") && !name.contains("@{")
}

fn assert_repo(project: &str, repo: &str) -> Result<(), String> {
    if is_safe_repo_segment(project) && is_safe_repo_segment(repo) {
        Ok(())
    } else {
        Err(format!("Not a safe Bitbucket project/repo: {project}/{repo}"))
    }
}

fn assert_branch(branch: &str) -> Result<(), String> {
    if is_safe_branch_name(branch) {
        Ok(())
    } else {
        Err(format!("Not a safe branch name: {}", slice_utf16(branch, 80)))
    }
}

/// `URLSearchParams` serialization (form encoding, spaces as `+`).
fn query(pairs: &[(&str, &str)]) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

pub fn repo_path(project: &str, repo: &str) -> Result<String, String> {
    assert_repo(project, repo)?;
    Ok(format!("{API_ROOT}/projects/{}/repos/{}", encode_uri_component(project), encode_uri_component(repo)))
}

/// Newest endpoint first; `/branches/default` is the pre-7.5 spelling.
pub fn default_branch_paths(project: &str, repo: &str) -> Result<Vec<String>, String> {
    let base = repo_path(project, repo)?;
    Ok(vec![format!("{base}/default-branch"), format!("{base}/branches/default")])
}

/// PRs whose source is `branch` in this repo (any state, newest first).
pub fn branch_pull_requests_path(project: &str, repo: &str, branch: &str, state: &str, limit: u32) -> Result<String, String> {
    assert_branch(branch)?;
    let q = query(&[("state", state), ("direction", "OUTGOING"), ("at", &format!("refs/heads/{branch}")), ("order", "NEWEST"), ("limit", &limit.to_string())]);
    Ok(format!("{}/pull-requests?{q}", repo_path(project, repo)?))
}

/// The build-status plugin. The `/commits/{sha}/builds` spelling isn't used:
/// on Bitbucket DC 9.4 it answers 400 ("non-blank key"), not 404, so a
/// fallback never happens. Kept as a list so callers still go through
/// `first_available`.
pub fn commit_builds_paths(sha: &str) -> Result<Vec<String>, String> {
    if !SHA_RE.is_match(sha) {
        return Err("Not a commit id.".into());
    }
    Ok(vec![format!("/rest/build-status/1.0/commits/{sha}")])
}

pub struct DefaultReviewersQuery<'a> {
    pub source_repo_id: i64,
    pub target_repo_id: i64,
    pub source_branch: &'a str,
    pub target_branch: &'a str,
}

fn is_safe_integer(n: i64) -> bool {
    n.abs() <= MAX_SAFE_INTEGER
}

/// REST PR creation doesn't add default reviewers, so they're asked for here.
pub fn default_reviewers_path(project: &str, repo: &str, q: &DefaultReviewersQuery<'_>) -> Result<String, String> {
    assert_repo(project, repo)?;
    assert_branch(q.source_branch)?;
    assert_branch(q.target_branch)?;
    if !is_safe_integer(q.source_repo_id) || !is_safe_integer(q.target_repo_id) {
        return Err("Not a repository id.".into());
    }
    let query = query(&[
        ("sourceRepoId", &q.source_repo_id.to_string()),
        ("targetRepoId", &q.target_repo_id.to_string()),
        ("sourceRefId", &format!("refs/heads/{}", q.source_branch)),
        ("targetRefId", &format!("refs/heads/{}", q.target_branch)),
    ]);
    Ok(format!("/rest/default-reviewers/1.0/projects/{}/repos/{}/reviewers?{query}", encode_uri_component(project), encode_uri_component(repo)))
}

pub fn create_pull_request_path(project: &str, repo: &str) -> Result<String, String> {
    Ok(format!("{}/pull-requests", repo_path(project, repo)?))
}

pub struct PullRequestDraft<'a> {
    pub project: &'a str,
    pub repo: &'a str,
    pub title: &'a Value,
    pub description: &'a Value,
    pub from_branch: &'a str,
    pub to_branch: &'a str,
    pub reviewers: &'a [String],
}

/// The POST body for a same-repo PR from `from_branch` into `to_branch`.
pub fn create_pull_request_body(d: &PullRequestDraft<'_>) -> Result<Value, String> {
    assert_repo(d.project, d.repo)?;
    assert_branch(d.from_branch)?;
    assert_branch(d.to_branch)?;
    let reference = |branch: &str| json!({ "id": format!("refs/heads/{branch}"), "repository": { "slug": d.repo, "project": { "key": d.project } } });
    let mut body = serde_json::Map::new();
    body.insert("title".into(), d.title.clone());
    body.insert("description".into(), d.description.clone());
    body.insert("fromRef".into(), reference(d.from_branch));
    body.insert("toRef".into(), reference(d.to_branch));
    body.insert("reviewers".into(), Value::Array(d.reviewers.iter().map(|name| json!({ "user": { "name": name } })).collect()));
    Ok(Value::Object(body))
}

/// `first_available` failed.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstAvailableError<E> {
    NoEndpoint,
    Failed(E),
}

/// Tries each path in turn with `fetch(path)` and returns the path and body of
/// the first that answers. Only a "not found" error (per `is_not_found`) moves
/// on to the next path; anything else (auth, 5xx) is returned at once, and so
/// is the last path's error.
pub async fn first_available<T, E, F, Fut>(paths: &[String], mut fetch: F, is_not_found: impl Fn(&E) -> bool) -> Result<(String, T), FirstAvailableError<E>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut last: Option<E> = None;
    for path in paths {
        match fetch(path.clone()).await {
            Ok(body) => return Ok((path.clone(), body)),
            Err(e) if is_not_found(&e) => last = Some(e),
            Err(e) => return Err(FirstAvailableError::Failed(e)),
        }
    }
    Err(last.map_or(FirstAvailableError::NoEndpoint, FirstAvailableError::Failed))
}

// ---- the watchers and the digest ----

const DASHBOARD_ROLES: [&str; 3] = ["AUTHOR", "REVIEWER", "PARTICIPANT"];
const PARTICIPANT_STATUSES: [&str; 3] = ["UNAPPROVED", "NEEDS_WORK", "APPROVED"];

/// Your open PRs across every repo: as author, or waiting on your review
/// (`participant_status: Some("UNAPPROVED")`).
pub fn dashboard_pull_requests_path(role: &str, participant_status: Option<&str>, limit: i64) -> Result<String, String> {
    if !DASHBOARD_ROLES.contains(&role) {
        return Err(format!("Not a dashboard role: {}", slice_utf16(role, 20)));
    }
    let limit = limit.clamp(1, 100).to_string();
    let mut pairs = vec![("role", role), ("state", "OPEN"), ("order", "NEWEST"), ("limit", limit.as_str())];
    if let Some(status) = participant_status {
        if !PARTICIPANT_STATUSES.contains(&status) {
            return Err("Not a participant status.".into());
        }
        pairs.push(("participantStatus", status));
    }
    Ok(format!("{API_ROOT}/dashboard/pull-requests?{}", query(&pairs)))
}

/// Whether a PR can merge: `{canMerge, conflicted, outcome, vetoes}`.
pub fn merge_status_path(project: &str, repo: &str, pr_id: i64) -> Result<String, String> {
    if !(1..=MAX_SAFE_INTEGER).contains(&pr_id) {
        return Err("Not a pull request id.".into());
    }
    Ok(format!("{}/pull-requests/{pr_id}/merge", repo_path(project, repo)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths_are_pinned_encoded_and_refuse_unsafe_segments() {
        assert_eq!(repo_path("ACME", "sample-app").unwrap(), "/rest/api/1.0/projects/ACME/repos/sample-app");
        assert_eq!(repo_path("a.b", "tools").unwrap(), "/rest/api/1.0/projects/a.b/repos/tools");
        for (p, r) in [("~jdoe", "x"), ("..", "x"), ("a/b", "x"), ("a", "x y"), ("", "x"), ("a", "../x")] {
            assert!(repo_path(p, r).unwrap_err().starts_with("Not a safe Bitbucket project/repo"), "{p}/{r}");
        }
        assert_eq!(
            default_branch_paths("ACME", "app").unwrap(),
            ["/rest/api/1.0/projects/ACME/repos/app/default-branch", "/rest/api/1.0/projects/ACME/repos/app/branches/default"]
        );
        assert_eq!(create_pull_request_path("ACME", "app").unwrap(), "/rest/api/1.0/projects/ACME/repos/app/pull-requests");
    }

    #[test]
    fn branch_names_are_plain() {
        for ok in ["main", "bugfix/PROJ-7-login", "release/1.2.3", "a_b", "x".repeat(200).as_str()] {
            assert!(is_safe_branch_name(ok), "{ok}");
        }
        for bad in ["-x", "a..b", "a//b", "a/", "a.", "a.lock", "a b", "a@{1}", "a'b", "", "a;rm", "x".repeat(201).as_str(), ".hidden", "é"] {
            assert!(!is_safe_branch_name(bad), "{bad}");
        }
    }

    #[test]
    fn branch_pull_requests_use_form_encoded_query_in_the_node_order() {
        assert_eq!(
            branch_pull_requests_path("ACME", "app", "bugfix/PROJ-7", "ALL", 10).unwrap(),
            "/rest/api/1.0/projects/ACME/repos/app/pull-requests?state=ALL&direction=OUTGOING&at=refs%2Fheads%2Fbugfix%2FPROJ-7&order=NEWEST&limit=10"
        );
        assert!(branch_pull_requests_path("ACME", "app", "a b", "ALL", 10).unwrap_err().starts_with("Not a safe branch name: a b"));
        assert!(branch_pull_requests_path("..", "app", "main", "ALL", 10).is_err());
        assert!(branch_pull_requests_path("ACME", "app", &"x".repeat(300), "ALL", 10).unwrap_err().len() < 120, "the message clips the branch name");
    }

    #[test]
    fn commit_builds_need_a_hex_commit_id() {
        assert_eq!(commit_builds_paths("abcdef1").unwrap(), ["/rest/build-status/1.0/commits/abcdef1"]);
        assert!(commit_builds_paths(&"A".repeat(40)).is_ok());
        for bad in ["abc", "xyzxyzxyz", &"a".repeat(41), "abcdef1/../x", ""] {
            assert_eq!(commit_builds_paths(bad).unwrap_err(), "Not a commit id.", "{bad}");
        }
    }

    #[test]
    fn default_reviewers_ask_for_both_repo_ids_and_both_refs() {
        let q = DefaultReviewersQuery { source_repo_id: 5, target_repo_id: 6, source_branch: "bugfix/x", target_branch: "master" };
        assert_eq!(
            default_reviewers_path("ACME", "app", &q).unwrap(),
            "/rest/default-reviewers/1.0/projects/ACME/repos/app/reviewers?sourceRepoId=5&targetRepoId=6&sourceRefId=refs%2Fheads%2Fbugfix%2Fx&targetRefId=refs%2Fheads%2Fmaster"
        );
        let huge = DefaultReviewersQuery { source_repo_id: 9_007_199_254_740_992, ..q };
        assert_eq!(default_reviewers_path("ACME", "app", &huge).unwrap_err(), "Not a repository id.");
        let bad = DefaultReviewersQuery { target_branch: "a b", source_repo_id: 1, target_repo_id: 2, source_branch: "x" };
        assert!(default_reviewers_path("ACME", "app", &bad).is_err());
    }

    #[test]
    fn create_pull_request_body_names_both_refs_and_the_reviewers_in_order() {
        let reviewers = vec!["ann".to_string(), "bo".to_string()];
        let body = create_pull_request_body(&PullRequestDraft { project: "ACME", repo: "app", title: &json!("T"), description: &json!("D"), from_branch: "bugfix/x", to_branch: "master", reviewers: &reviewers }).unwrap();
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"title":"T","description":"D","fromRef":{"id":"refs/heads/bugfix/x","repository":{"slug":"app","project":{"key":"ACME"}}},"toRef":{"id":"refs/heads/master","repository":{"slug":"app","project":{"key":"ACME"}}},"reviewers":[{"user":{"name":"ann"}},{"user":{"name":"bo"}}]}"#
        );
        let none = create_pull_request_body(&PullRequestDraft { project: "ACME", repo: "app", title: &json!("T"), description: &Value::Null, from_branch: "x", to_branch: "y", reviewers: &[] }).unwrap();
        assert_eq!(none["reviewers"], json!([]));
        assert!(create_pull_request_body(&PullRequestDraft { project: "ACME", repo: "app", title: &json!("T"), description: &json!("D"), from_branch: "-x", to_branch: "y", reviewers: &[] }).is_err());
    }

    #[test]
    fn dashboard_paths_validate_role_status_and_clamp_the_limit() {
        assert_eq!(dashboard_pull_requests_path("AUTHOR", None, 50).unwrap(), "/rest/api/1.0/dashboard/pull-requests?role=AUTHOR&state=OPEN&order=NEWEST&limit=50");
        assert_eq!(
            dashboard_pull_requests_path("REVIEWER", Some("UNAPPROVED"), 500).unwrap(),
            "/rest/api/1.0/dashboard/pull-requests?role=REVIEWER&state=OPEN&order=NEWEST&limit=100&participantStatus=UNAPPROVED"
        );
        assert!(dashboard_pull_requests_path("AUTHOR", None, -3).unwrap().ends_with("limit=1"));
        assert!(dashboard_pull_requests_path("ADMIN", None, 50).unwrap_err().starts_with("Not a dashboard role: ADMIN"));
        assert_eq!(dashboard_pull_requests_path("AUTHOR", Some("MERGED"), 50).unwrap_err(), "Not a participant status.");
    }

    #[test]
    fn merge_status_needs_a_positive_pull_request_id() {
        assert_eq!(merge_status_path("ACME", "app", 7).unwrap(), "/rest/api/1.0/projects/ACME/repos/app/pull-requests/7/merge");
        for bad in [0, -1, 9_007_199_254_740_992] {
            assert_eq!(merge_status_path("ACME", "app", bad).unwrap_err(), "Not a pull request id.");
        }
    }

    type Fetched = Result<&'static str, u16>;

    async fn first(paths: &[&str], answers: Vec<Fetched>) -> (Result<(String, &'static str), FirstAvailableError<u16>>, usize) {
        let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let queue = std::sync::Mutex::new(std::collections::VecDeque::from(answers));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = first_available(
            &paths,
            |_p| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let next = queue.lock().unwrap().pop_front().unwrap();
                async move { next }
            },
            |e| *e == 404,
        )
        .await;
        (out, calls.load(std::sync::atomic::Ordering::SeqCst))
    }

    #[tokio::test]
    async fn first_available_moves_on_only_for_not_found() {
        let (ok, calls) = first(&["/a", "/b"], vec![Err(404), Ok("body")]).await;
        assert_eq!(ok, Ok(("/b".to_string(), "body")));
        assert_eq!(calls, 2);
        let (first_ok, calls) = first(&["/a", "/b"], vec![Ok("x")]).await;
        assert_eq!(first_ok, Ok(("/a".to_string(), "x")));
        assert_eq!(calls, 1);
        let (auth, calls) = first(&["/a", "/b"], vec![Err(401)]).await;
        assert_eq!(auth, Err(FirstAvailableError::Failed(401)));
        assert_eq!(calls, 1, "anything but not-found is thrown at once");
        let (all_missing, _) = first(&["/a", "/b"], vec![Err(404), Err(404)]).await;
        assert_eq!(all_missing, Err(FirstAvailableError::Failed(404)));
        let (none, _) = first(&[], vec![]).await;
        assert_eq!(none, Err(FirstAvailableError::NoEndpoint));
    }
}
