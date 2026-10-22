//! GitHub's JSON turned into the shapes the features already read (the same
//! ones `bitbucket_normalize` produces for Bitbucket), so nothing downstream
//! knows which host it is talking to (port of `core/github-normalize.js`).
//! Pure and defensive: a missing or mistyped field becomes null (or an
//! "unknown" enum), never a panic.
//!
//! Mapping notes:
//!  - project = the repository owner, repo = the repository name, prId = the
//!    pull request number.
//!  - state = "OPEN" | "MERGED" | "DECLINED" (GitHub's closed-without-merge).
//!  - logins are lower-cased wherever they are compared (`authorSlug`), as
//!    GitHub logins are case-insensitive.

use crate::bitbucket_normalize::{
    Anchor, Build, BuildCounts, BuildStatuses, MergeStatus, PullRequest, RefRepo, Repository, ReviewComment,
};
use crate::js_date::parse_date_ms;
use crate::js_text::js_to_string;
use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::collections::HashSet;

fn obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// A non-empty string, else `None`.
fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn num(v: Option<&Value>) -> Option<Number> {
    match v {
        Some(Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => Some(n.clone()),
        _ => None,
    }
}

/// The lower-cased `login` of a user object.
fn login(user: Option<&Value>) -> Option<String> {
    str_of(obj(user).and_then(|u| u.get("login"))).map(|l| l.to_lowercase())
}

/// An ISO timestamp as epoch milliseconds.
fn ms(iso: Option<&Value>) -> Option<i64> {
    parse_date_ms(iso.and_then(Value::as_str)?)
}

fn ms_number(iso: Option<&Value>) -> Option<Number> {
    ms(iso).map(Number::from)
}

/// `{ projectKey, slug }` of a repository object (`{ name, owner: { login } }`);
/// both null when it is gone (a deleted fork).
fn repo_ref(repo: Option<&Value>) -> RefRepo {
    let Some(r) = obj(repo) else { return RefRepo { project_key: None, slug: None } };
    RefRepo { project_key: str_of(obj(r.get("owner")).and_then(|o| o.get("login"))), slug: str_of(r.get("name")) }
}

fn state_of(pr: &Map<String, Value>) -> Option<String> {
    if pr.get("merged") == Some(&Value::Bool(true)) || str_of(pr.get("merged_at")).is_some() {
        return Some("MERGED".into());
    }
    match pr.get("state").and_then(Value::as_str) {
        Some("closed") => Some("DECLINED".into()),
        Some("open") => Some("OPEN".into()),
        _ => None,
    }
}

/// Sum of two optional numbers (a missing one counts as 0), kept an integer
/// when both are.
fn sum(a: Option<&Number>, b: Option<&Number>) -> Number {
    let (x, y) = (a.map_or(0.0, |n| n.as_f64().unwrap_or(0.0)), b.map_or(0.0, |n| n.as_f64().unwrap_or(0.0)));
    let total = x + y;
    if total.fract() == 0.0 && total.abs() < 9e15 { Number::from(total as i64) } else { Number::from_f64(total).unwrap_or_else(|| Number::from(0)) }
}

/// The PR as `getPullRequest` returns it (REST `GET /pulls/{n}`).
pub fn normalize_pull_request(raw: &Value) -> Option<PullRequest> {
    let raw = raw.as_object()?;
    let head = obj(raw.get("head"));
    let base = obj(raw.get("base"));
    let (comments, review_comments) = (num(raw.get("comments")), num(raw.get("review_comments")));
    let comment_count =
        (comments.is_some() || review_comments.is_some()).then(|| sum(comments.as_ref(), review_comments.as_ref()));
    Some(PullRequest {
        id: num(raw.get("number")),
        version: None,
        title: str_of(raw.get("title")),
        state: state_of(raw),
        from_branch: str_of(head.and_then(|h| h.get("ref"))),
        from_sha: str_of(head.and_then(|h| h.get("sha"))),
        from_repo: repo_ref(head.and_then(|h| h.get("repo"))),
        to_branch: str_of(base.and_then(|b| b.get("ref"))),
        to_sha: str_of(base.and_then(|b| b.get("sha"))),
        to_repo: repo_ref(base.and_then(|b| b.get("repo"))),
        author_slug: login(raw.get("user")),
        open_task_count: None,
        comment_count,
    })
}

/// A PR as the ticket features list it: the normalized PR plus its address and
/// last update.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestSummary {
    pub id: Number,
    pub title: Option<String>,
    pub state: Option<String>,
    pub from_branch: Option<String>,
    pub from_sha: Option<String>,
    pub to_branch: Option<String>,
    pub url: Option<String>,
    pub updated_at: Option<i64>,
}

pub fn normalize_pull_request_summary(raw: &Value) -> Option<PullRequestSummary> {
    let pr = normalize_pull_request(raw)?;
    Some(PullRequestSummary {
        id: pr.id?,
        title: pr.title,
        state: pr.state,
        from_branch: pr.from_branch,
        from_sha: pr.from_sha,
        to_branch: pr.to_branch,
        url: str_of(raw.get("html_url")),
        updated_at: ms(raw.get("updated_at")),
    })
}

pub fn normalize_pull_request_list(raw: &Value) -> Vec<PullRequestSummary> {
    raw.as_array().map(Vec::as_slice).unwrap_or(&[]).iter().filter_map(normalize_pull_request_summary).collect()
}

/// `GET /pulls/{n}`'s merge fields. `mergeable` is null while GitHub is still
/// working it out.
pub fn normalize_merge_status(raw: &Value) -> MergeStatus {
    let Some(r) = raw.as_object() else { return MergeStatus { conflicted: None, can_merge: None, outcome: None } };
    let state = str_of(r.get("mergeable_state"));
    let mergeable = r.get("mergeable");
    let st = state.as_deref();
    let conflicted = if mergeable == Some(&Value::Bool(false)) || st == Some("dirty") {
        Some(true)
    } else if mergeable == Some(&Value::Bool(true)) {
        Some(false)
    } else {
        None
    };
    let can_merge = if mergeable == Some(&Value::Bool(true)) && matches!(st, Some("clean" | "unstable" | "has_hooks")) {
        Some(true)
    } else if mergeable == Some(&Value::Bool(false)) || matches!(st, Some("blocked" | "dirty" | "behind" | "draft")) {
        Some(false)
    } else {
        None
    };
    MergeStatus { conflicted, can_merge, outcome: state.map(|s| s.to_uppercase()) }
}

/// `GET /repos/{o}/{r}`'s id, name and owner.
pub fn normalize_repository(raw: &Value) -> Option<Repository> {
    let id = num(raw.as_object()?.get("id"))?;
    let r = repo_ref(Some(raw));
    Some(Repository { id, slug: r.slug, project_key: r.project_key })
}

pub fn normalize_default_branch(raw: &Value) -> Option<String> {
    str_of(obj(Some(raw))?.get("default_branch"))
}

// ---- Review threads (GraphQL) ----

fn normalize_thread_comment(c: &Map<String, Value>) -> ReviewComment {
    ReviewComment {
        id: num(c.get("databaseId")),
        version: None,
        text: str_of(c.get("body")),
        author_slug: login(c.get("author")),
        author_name: str_of(obj(c.get("author")).and_then(|a| a.get("login"))),
        created_at: ms_number(c.get("createdAt")),
        severity: "NORMAL".into(),
        state: "OPEN".into(),
        thread_resolved: None,
        anchor: None,
        replies: vec![],
    }
}

/// One review thread as a ReviewComment: the first comment is the root and the
/// rest are its replies. A resolved thread says so (state "RESOLVED"); an
/// outdated one (its lines changed) is "orphaned", which the
/// address-review-comments feature skips, as it skips Bitbucket's.
pub fn normalize_review_thread(thread: &Value) -> Option<ReviewComment> {
    let t = thread.as_object()?;
    let comments: Vec<&Map<String, Value>> = obj(t.get("comments"))
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    let (first, rest) = comments.split_first()?;
    let mut root = normalize_thread_comment(first);
    root.id.as_ref()?;
    let resolved = t.get("isResolved") == Some(&Value::Bool(true));
    let left = t.get("diffSide").and_then(Value::as_str) == Some("LEFT");
    root.state = if resolved { "RESOLVED" } else { "OPEN" }.into();
    root.thread_resolved = t.get("isResolved").and_then(Value::as_bool);
    root.anchor = Some(Anchor {
        path: str_of(t.get("path")),
        line: num(t.get("line")).or_else(|| num(t.get("originalLine"))),
        line_type: if left { "REMOVED" } else { "unknown" }.into(),
        file_type: if left { "FROM" } else { "TO" }.into(),
        diff_type: "EFFECTIVE".into(),
        orphaned: t.get("isOutdated") == Some(&Value::Bool(true)),
    });
    root.replies = rest.iter().map(|c| normalize_thread_comment(c)).filter(|c| c.id.is_some()).collect();
    Some(root)
}

fn dig<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |acc, k| acc.as_object().and_then(|o| o.get(*k)))
}

/// The ReviewComment list of one or more GraphQL pages
/// (`data.repository.pullRequest.reviewThreads`), deduped by id.
pub fn normalize_review_threads(pages: &Value) -> Vec<ReviewComment> {
    let single;
    let page_list: &[Value] = match pages {
        Value::Array(a) => a,
        other => {
            single = [other.clone()];
            &single
        }
    };
    let mut seen: HashSet<u64> = HashSet::new();
    let mut out = Vec::new();
    for page in page_list {
        let nodes = dig(page, &["data", "repository", "pullRequest", "reviewThreads", "nodes"]).and_then(Value::as_array);
        for node in nodes.map(Vec::as_slice).unwrap_or(&[]) {
            let Some(comment) = normalize_review_thread(node) else { continue };
            let key = comment.id.as_ref().and_then(Number::as_f64).unwrap_or(0.0).to_bits();
            if seen.insert(key) {
                out.push(comment);
            }
        }
    }
    out
}

// ---- Build state ----

fn check_run_state(run: &Map<String, Value>) -> &'static str {
    if run.get("status").and_then(Value::as_str) != Some("completed") {
        return "INPROGRESS";
    }
    match run.get("conclusion").and_then(Value::as_str) {
        Some("success" | "neutral" | "skipped") => "SUCCESSFUL",
        Some("failure" | "timed_out" | "cancelled" | "action_required" | "stale" | "startup_failure") => "FAILED",
        _ => "unknown",
    }
}

fn status_state(s: Option<&str>) -> &'static str {
    match s {
        Some("success") => "SUCCESSFUL",
        Some("pending") => "INPROGRESS",
        Some("failure" | "error") => "FAILED",
        _ => "unknown",
    }
}

/// A commit's builds: its check runs and its legacy commit statuses together,
/// worst state first (FAILED, then in progress, then successful: an unknown
/// state counts as in progress, never green). `None` when it has neither. Same
/// shape as Bitbucket's build summary.
pub fn normalize_build_status(check_runs: &Value, status: &Value) -> Option<BuildStatuses> {
    let mut builds: Vec<Build> = Vec::new();
    let runs = check_runs.get("check_runs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    for run in runs.iter().filter_map(Value::as_object) {
        builds.push(Build {
            state: check_run_state(run).into(),
            key: num(run.get("id")).map(|n| js_to_string(&Value::Number(n))),
            name: str_of(run.get("name")),
            url: str_of(run.get("html_url")),
            at: ms_number(run.get("completed_at")).or_else(|| ms_number(run.get("started_at"))),
        });
    }
    let statuses = status.get("statuses").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    for s in statuses.iter().filter_map(Value::as_object) {
        builds.push(Build {
            state: status_state(s.get("state").and_then(Value::as_str)).into(),
            key: str_of(s.get("context")),
            name: str_of(s.get("context")),
            url: str_of(s.get("target_url")),
            at: ms_number(s.get("updated_at")),
        });
    }
    if builds.is_empty() {
        return None;
    }
    let mut counts = BuildCounts::default();
    for b in &builds {
        match b.state.as_str() {
            "SUCCESSFUL" => counts.successful += 1,
            "FAILED" => counts.failed += 1,
            "INPROGRESS" => counts.inprogress += 1,
            _ => counts.unknown += 1,
        }
    }
    let state = if counts.failed > 0 {
        "FAILED"
    } else if counts.inprogress + counts.unknown > 0 {
        "INPROGRESS"
    } else {
        "SUCCESSFUL"
    };
    Some(BuildStatuses { state: Some(state.into()), counts, builds: builds.into_iter().take(10).collect() })
}

// ---- Dashboard (GraphQL search) ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reviewer {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardPullRequest {
    pub id: Number,
    pub title: Option<String>,
    pub state: String,
    pub from_branch: Option<String>,
    pub from_sha: Option<String>,
    pub to_branch: Option<String>,
    pub url: Option<String>,
    pub updated_at: Option<i64>,
    pub project: String,
    pub repo: String,
    pub to_sha: Option<String>,
    pub from_repo: RefRepo,
    pub author_slug: Option<String>,
    pub reviewers: Vec<Reviewer>,
    pub approvals: usize,
}

pub fn normalize_dashboard_pull_request(node: &Value) -> Option<DashboardPullRequest> {
    let n = node.as_object()?;
    let id = num(n.get("number"))?;
    let repository = repo_ref(n.get("repository"));
    let (project, repo) = (repository.project_key?, repository.slug?);

    // Insertion-ordered: setting a name that exists updates it in place.
    let mut reviewers: Vec<Reviewer> = Vec::new();
    let mut set = |name: String, status: &str| match reviewers.iter_mut().find(|r| r.name == name) {
        Some(r) => r.status = status.into(),
        None => reviewers.push(Reviewer { name, status: status.into() }),
    };
    let nodes_of = |key: &str| {
        obj(n.get(key)).and_then(|o| o.get("nodes")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).to_vec()
    };
    for r in nodes_of("reviewRequests") {
        // A team review request has no login and is skipped.
        if let Some(name) = obj(Some(&r)).and_then(|r| login(r.get("requestedReviewer"))) {
            set(name, "UNAPPROVED");
        }
    }
    for r in nodes_of("latestReviews") {
        let Some(ro) = r.as_object() else { continue };
        if let Some(name) = login(ro.get("author")) {
            let status = match ro.get("state").and_then(Value::as_str) {
                Some("APPROVED") => "APPROVED",
                Some("CHANGES_REQUESTED") => "NEEDS_WORK",
                _ => "UNAPPROVED",
            };
            set(name, status);
        }
    }
    let approvals = reviewers.iter().filter(|r| r.status == "APPROVED").count();
    Some(DashboardPullRequest {
        id,
        title: str_of(n.get("title")),
        state: "OPEN".into(),
        from_branch: str_of(n.get("headRefName")),
        from_sha: str_of(n.get("headRefOid")),
        to_branch: str_of(n.get("baseRefName")),
        url: str_of(n.get("url")),
        updated_at: ms(n.get("updatedAt")),
        project,
        repo,
        to_sha: str_of(n.get("baseRefOid")),
        from_repo: repo_ref(n.get("headRepository")),
        author_slug: login(n.get("author")),
        reviewers,
        approvals,
    })
}

/// The open PRs of a GraphQL search result (`data.search.nodes`); anything that
/// isn't a usable PR is dropped.
pub fn normalize_dashboard(raw: &Value) -> Vec<DashboardPullRequest> {
    let nodes = dig(raw, &["data", "search", "nodes"]).and_then(Value::as_array);
    nodes.map(Vec::as_slice).unwrap_or(&[]).iter().filter_map(normalize_dashboard_pull_request).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github")
    }

    fn fx(name: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(dir().join(name)).unwrap()).unwrap()
    }

    fn ser<T: Serialize>(v: T) -> Value {
        serde_json::to_value(v).unwrap()
    }

    /// The Node implementation's own output for every fixture, as JSON values:
    /// the strongest check that the port changed no behavior.
    #[test]
    fn output_matches_what_the_node_implementation_produced_for_every_fixture() {
        let golden = fx("golden.json");
        let g = |k: &str| golden[k].clone();
        for f in ["pull-request.json", "pull-request-merged.json", "pull-request-fork.json", "pull-request-deleted-fork.json"] {
            let raw = fx(f);
            assert_eq!(ser(normalize_pull_request(&raw)), g(&format!("pr:{f}")), "pr {f}");
            assert_eq!(ser(normalize_pull_request_summary(&raw)), g(&format!("summary:{f}")), "summary {f}");
            assert_eq!(ser(normalize_merge_status(&raw)), g(&format!("merge:{f}")), "merge {f}");
        }
        assert_eq!(ser(normalize_pull_request_list(&fx("pulls-list.json"))), g("list"));
        assert_eq!(ser(normalize_repository(&fx("repo.json"))), g("repo"));
        assert_eq!(ser(normalize_default_branch(&fx("repo.json"))), g("defaultBranch"));
        let (p1, p2) = (fx("review-threads-page1.json"), fx("review-threads-page2.json"));
        assert_eq!(ser(normalize_review_threads(&json!([p1.clone()]))), g("threads1"));
        assert_eq!(ser(normalize_review_threads(&json!([p2.clone()]))), g("threads2"));
        assert_eq!(ser(normalize_review_threads(&json!([p1, p2]))), g("threads12"));
        assert_eq!(ser(normalize_build_status(&fx("check-runs.json"), &fx("status.json"))), g("builds"));
        assert_eq!(ser(normalize_dashboard(&fx("dashboard.json"))), g("dashboard"));
    }

    fn n(v: i64) -> Option<Number> {
        Some(Number::from(v))
    }

    // ---- pull requests ----

    #[test]
    fn a_pr_maps_onto_the_normalized_shape() {
        assert_eq!(
            ser(normalize_pull_request(&fx("pull-request.json"))),
            json!({
                "id": 42, "version": null, "title": "Fix the flaky login test", "state": "OPEN",
                "fromBranch": "fix/login-test", "fromSha": "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
                "fromRepo": { "projectKey": "Octo", "slug": "hello" },
                "toBranch": "main", "toSha": "0f1e2d3c4b5a69788796a5b4c3d2e1f001122334",
                "toRepo": { "projectKey": "Octo", "slug": "hello" },
                "authorSlug": "hubot", "commentCount": 8,
            })
        );
    }

    #[test]
    fn state_merged_closed_without_merging_and_open() {
        let s = |f: &str| normalize_pull_request(&fx(f)).unwrap().state;
        assert_eq!(s("pull-request-merged.json").as_deref(), Some("MERGED"));
        assert_eq!(s("pull-request-deleted-fork.json").as_deref(), Some("DECLINED"));
        assert_eq!(s("pull-request.json").as_deref(), Some("OPEN"));
        assert_eq!(normalize_pull_request(&json!({ "number": 1, "state": "weird" })).unwrap().state, None);
        assert_eq!(normalize_pull_request(&json!({ "state": "open", "merged": true })).unwrap().state.as_deref(), Some("MERGED"));
        assert_eq!(normalize_pull_request(&json!({ "state": "open", "merged_at": "2026-01-01T00:00:00Z" })).unwrap().state.as_deref(), Some("MERGED"));
    }

    #[test]
    fn a_fork_pr_names_the_fork_as_its_source_a_deleted_fork_leaves_it_empty() {
        let from = |f: &str| normalize_pull_request(&fx(f)).unwrap().from_repo;
        assert_eq!(from("pull-request-fork.json"), RefRepo { project_key: Some("contributor".into()), slug: Some("hello".into()) });
        assert_eq!(from("pull-request-deleted-fork.json"), RefRepo { project_key: None, slug: None });
    }

    #[test]
    fn junk_input_gives_none_or_null_fields_and_comment_counts_add_up() {
        for junk in [json!(null), json!(5), json!("x"), json!([]), json!({})] {
            let _ = normalize_pull_request(&junk);
        }
        assert!(normalize_pull_request(&json!(null)).is_none());
        assert_eq!(normalize_pull_request(&json!({})).unwrap().id, None);
        assert!(normalize_pull_request_summary(&json!({ "title": "no number" })).is_none());
        let count = |v: Value| normalize_pull_request(&v).unwrap().comment_count;
        assert_eq!(count(json!({})), None);
        assert_eq!(count(json!({ "comments": 3 })), n(3));
        assert_eq!(count(json!({ "review_comments": 4 })), n(4));
        assert_eq!(count(json!({ "comments": 3, "review_comments": 4 })), n(7));
        assert_eq!(count(json!({ "comments": "3" })), None);
    }

    #[test]
    fn a_pr_list_keeps_each_prs_address_and_last_update_and_drops_entries_without_a_number() {
        let list = normalize_pull_request_list(&fx("pulls-list.json"));
        assert_eq!(list.iter().map(|p| p.id.clone()).collect::<Vec<_>>(), [Number::from(42), Number::from(40)]);
        assert_eq!(list[0].url.as_deref(), Some("https://github.com/Octo/hello/pull/42"));
        assert_eq!(list[0].updated_at, parse_date_ms("2026-09-30T12:34:56Z"));
        assert_eq!(list[1].state.as_deref(), Some("MERGED"));
        assert!(normalize_pull_request_list(&json!("nope")).is_empty());
    }

    // ---- merge status ----

    #[test]
    fn merge_status_conflicted_clean_still_computing_blocked() {
        let m = |v: Value| ser(normalize_merge_status(&v));
        assert_eq!(m(fx("pull-request.json")), json!({ "conflicted": true, "canMerge": false, "outcome": "DIRTY" }));
        assert_eq!(m(json!({ "mergeable": true, "mergeable_state": "clean" })), json!({ "conflicted": false, "canMerge": true, "outcome": "CLEAN" }));
        assert_eq!(m(fx("pull-request-fork.json")), json!({ "conflicted": null, "canMerge": null, "outcome": "UNKNOWN" }));
        assert_eq!(m(json!({ "mergeable": true, "mergeable_state": "blocked" })), json!({ "conflicted": false, "canMerge": false, "outcome": "BLOCKED" }));
        assert_eq!(m(json!(null)), json!({ "conflicted": null, "canMerge": null, "outcome": null }));
        assert_eq!(m(json!({ "mergeable": true, "mergeable_state": "unstable" }))["canMerge"], json!(true));
        assert_eq!(m(json!({ "mergeable": null, "mergeable_state": "behind" }))["canMerge"], json!(false));
        assert_eq!(m(json!({ "mergeable": false }))["conflicted"], json!(true));
    }

    #[test]
    fn repository_and_default_branch() {
        assert_eq!(ser(normalize_repository(&fx("repo.json"))), json!({ "id": 101, "slug": "hello", "projectKey": "Octo" }));
        assert!(normalize_repository(&json!({})).is_none());
        assert_eq!(normalize_default_branch(&fx("repo.json")).as_deref(), Some("main"));
        assert_eq!(normalize_default_branch(&json!(null)), None);
    }

    // ---- review threads ----

    #[test]
    fn review_threads_become_comments_the_first_is_the_root_the_rest_are_replies() {
        let comments = normalize_review_threads(&json!([fx("review-threads-page1.json")]));
        let root = comments.iter().find(|c| c.id == n(1001)).unwrap();
        assert_eq!(root.text.as_deref(), Some("Please handle the empty token."));
        assert_eq!((root.author_slug.as_deref(), root.author_name.as_deref()), (Some("reviewer-one"), Some("Reviewer-One")));
        assert_eq!(root.created_at, ms_number(Some(&json!("2026-09-29T10:00:00Z"))));
        assert_eq!((root.severity.as_str(), root.state.as_str(), root.thread_resolved), ("NORMAL", "OPEN", Some(false)));
        assert_eq!(
            ser(&root.anchor),
            json!({ "path": "src/login.js", "line": 42, "lineType": "unknown", "fileType": "TO", "diffType": "EFFECTIVE", "orphaned": false })
        );
        let replies: Vec<_> = root.replies.iter().map(|r| (r.id.clone(), r.author_slug.clone())).collect();
        assert_eq!(replies, [(n(1002), Some("hubot".to_string()))]);
        assert!(root.replies[0].anchor.is_none());
    }

    #[test]
    fn a_resolved_thread_says_so_and_an_outdated_one_is_orphaned() {
        let comments = normalize_review_threads(&json!([fx("review-threads-page1.json")]));
        let resolved = comments.iter().find(|c| c.id == n(1003)).unwrap();
        assert_eq!((resolved.state.as_str(), resolved.thread_resolved), ("RESOLVED", Some(true)));
        let outdated = comments.iter().find(|c| c.id == n(1004)).unwrap().anchor.as_ref().unwrap();
        assert!(outdated.orphaned);
        assert_eq!(outdated.line, n(12), "falls back to the original line when the current one is gone");
    }

    #[test]
    fn a_comment_on_the_removed_side_is_marked_as_such() {
        let comments = normalize_review_threads(&json!([fx("review-threads-page2.json")]));
        let a = comments.iter().find(|c| c.id == n(1005)).unwrap().anchor.as_ref().unwrap();
        assert_eq!((a.line_type.as_str(), a.file_type.as_str()), ("REMOVED", "FROM"));
    }

    #[test]
    fn pages_are_merged_a_thread_seen_on_two_pages_counts_once_and_empty_threads_are_dropped() {
        let both = normalize_review_threads(&json!([fx("review-threads-page1.json"), fx("review-threads-page2.json")]));
        let mut ids: Vec<f64> = both.iter().map(|c| c.id.as_ref().unwrap().as_f64().unwrap()).collect();
        ids.sort_by(f64::total_cmp);
        assert_eq!(ids, [1001.0, 1003.0, 1004.0, 1005.0]);
        assert!(normalize_review_threads(&json!([])).is_empty());
        assert!(normalize_review_threads(&json!([{ "errors": [{ "message": "x" }] }])).is_empty());
        // A single page object (not wrapped in an array) is accepted, like the Node version.
        assert_eq!(normalize_review_threads(&fx("review-threads-page1.json")).len(), normalize_review_threads(&json!([fx("review-threads-page1.json")])).len());
        assert!(normalize_review_threads(&json!(null)).is_empty());
    }

    // ---- builds ----

    #[test]
    fn builds_check_runs_and_commit_statuses_together_the_worst_state_wins() {
        let s = normalize_build_status(&fx("check-runs.json"), &fx("status.json")).unwrap();
        assert_eq!(s.state.as_deref(), Some("FAILED"));
        assert_eq!(ser(&s.counts), json!({ "SUCCESSFUL": 2, "FAILED": 1, "INPROGRESS": 2, "unknown": 0 }));
        let lint = s.builds.iter().find(|b| b.name.as_deref() == Some("lint")).unwrap();
        assert_eq!(
            ser(lint),
            json!({ "state": "FAILED", "key": "12", "name": "lint", "url": "https://github.com/Octo/hello/runs/12", "at": parse_date_ms("2026-09-30T10:02:00Z") })
        );
    }

    #[test]
    fn builds_in_progress_beats_successful_and_an_unknown_state_is_never_green() {
        let state = |runs: Value| normalize_build_status(&json!({ "check_runs": runs }), &Value::Null).unwrap();
        let ok = json!({ "id": 1, "name": "a", "status": "completed", "conclusion": "success" });
        assert_eq!(state(json!([ok])).state.as_deref(), Some("SUCCESSFUL"));
        let running = state(json!([ok, { "id": 2, "name": "b", "status": "queued", "conclusion": null }]));
        assert_eq!(running.state.as_deref(), Some("INPROGRESS"));
        let odd = state(json!([ok, { "id": 3, "name": "c", "status": "completed", "conclusion": "something-new" }]));
        assert_eq!((odd.state.as_deref(), odd.counts.unknown), (Some("INPROGRESS"), 1));
    }

    #[test]
    fn builds_none_at_all_is_none_and_at_most_ten_are_listed() {
        assert!(normalize_build_status(&Value::Null, &Value::Null).is_none());
        assert!(normalize_build_status(&json!({ "check_runs": [] }), &json!({ "statuses": [] })).is_none());
        let many: Vec<Value> = (0..15).map(|i| json!({ "id": i, "name": format!("j{i}"), "status": "completed", "conclusion": "success" })).collect();
        let s = normalize_build_status(&json!({ "check_runs": many }), &Value::Null).unwrap();
        assert_eq!((s.builds.len(), s.counts.successful), (10, 15));
    }

    // ---- dashboard ----

    #[test]
    fn dashboard_prs_carry_their_repository_shas_source_repo_reviewers_and_approvals() {
        let prs = normalize_dashboard(&fx("dashboard.json"));
        assert_eq!(prs.iter().map(|p| p.id.clone()).collect::<Vec<_>>(), [Number::from(7), Number::from(8), Number::from(10)], "entries without a repository or a number are dropped");
        let first = &prs[0];
        assert_eq!((first.project.as_str(), first.repo.as_str(), first.state.as_str()), ("Octo", "hello", "OPEN"));
        assert_eq!((first.from_branch.as_deref(), first.to_branch.as_deref()), (Some("feat/retries"), Some("main")));
        assert_eq!((first.from_sha.clone(), first.to_sha.clone()), (Some("e".repeat(40)), Some("f".repeat(40))));
        assert_eq!((first.author_slug.as_deref(), first.url.as_deref()), (Some("hubot"), Some("https://github.com/Octo/hello/pull/7")));
        assert_eq!(first.from_repo, RefRepo { project_key: Some("Octo".into()), slug: Some("hello".into()) });
        assert_eq!(first.approvals, 1);
        let by_name: std::collections::BTreeMap<&str, &str> = first.reviewers.iter().map(|r| (r.name.as_str(), r.status.as_str())).collect();
        assert_eq!(by_name, [("reviewer-three", "UNAPPROVED"), ("reviewer-one", "APPROVED"), ("reviewer-two", "NEEDS_WORK")].into_iter().collect());
    }

    #[test]
    fn dashboard_a_fork_shows_as_the_source_repo_and_a_team_review_request_is_skipped() {
        let prs = normalize_dashboard(&fx("dashboard.json"));
        let by = |id: i64| prs.iter().find(|p| p.id == Number::from(id)).unwrap();
        assert_eq!(by(8).from_repo, RefRepo { project_key: Some("someone".into()), slug: Some("hello".into()) });
        assert_eq!(by(10).reviewers, [Reviewer { name: "reviewer-one".into(), status: "UNAPPROVED".into() }]);
        assert!(normalize_dashboard(&json!(null)).is_empty());
        assert!(normalize_dashboard(&json!({ "data": { "search": { "nodes": "no" } } })).is_empty());
    }
}
