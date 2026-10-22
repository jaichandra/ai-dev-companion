//! GitHub's REST and GraphQL addresses and queries for the git host provider
//! (port of `core/github-endpoints.js`). Pure: builds paths and bodies, and
//! refuses anything unsafe before a request exists; the owner, repo and branch
//! names reach a URL path, a search query and (from the clone) a git command
//! line.
//!
//! The web address (https://github.com, or https://ghe.example.com) and the
//! API address differ:
//!   github.com        REST https://api.github.com            GraphQL https://api.github.com/graphql
//!   GitHub Enterprise REST https://<host>/api/v3             GraphQL https://<host>/api/graphql

use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::js_text::slice_utf16;
use crate::prereqs::is_safe_repo_segment;
use crate::uri::encode_uri_component as enc;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;
use url::Url;

pub const API_VERSION: &str = "2022-11-28";
/// Headers every API call carries.
pub const API_HEADERS: [(&str, &str); 2] = [("Accept", "application/vnd.github+json"), ("X-GitHub-Api-Version", API_VERSION)];

static SHA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{7,40}$").unwrap());
static LOGIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9-]{0,38}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBases {
    pub rest: String,
    pub graphql: String,
}

/// The REST and GraphQL base URLs for a web base URL such as "https://github.com" (no trailing slash).
pub fn api_bases(web_base_url: &str) -> Result<ApiBases, String> {
    let url = Url::parse(web_base_url).map_err(|e| format!("Invalid URL: {e}"))?;
    let host = url.host_str().unwrap_or("").to_lowercase();
    if host == "github.com" || host == "www.github.com" {
        return Ok(ApiBases { rest: "https://api.github.com".into(), graphql: "https://api.github.com/graphql".into() });
    }
    let origin = url.origin().ascii_serialization();
    Ok(ApiBases { rest: format!("{origin}/api/v3"), graphql: format!("{origin}/api/graphql") })
}

pub fn assert_repo(owner: &str, repo: &str) -> Result<(), String> {
    if is_safe_repo_segment(owner) && is_safe_repo_segment(repo) {
        Ok(())
    } else {
        Err(format!("Not a safe GitHub owner/repo: {}/{}", slice_utf16(owner, 40), slice_utf16(repo, 40)))
    }
}

fn assert_pr_number(n: i64) -> Result<(), String> {
    if n >= 1 { Ok(()) } else { Err(format!("Not a pull request number: {n}")) }
}

fn assert_branch(branch: &str) -> Result<(), String> {
    if is_safe_branch_name(branch) { Ok(()) } else { Err(format!("Not a safe branch name: {}", slice_utf16(branch, 80))) }
}

fn repo_base(owner: &str, repo: &str) -> Result<String, String> {
    assert_repo(owner, repo)?;
    Ok(format!("/repos/{}/{}", enc(owner), enc(repo)))
}

pub fn user_path() -> &'static str {
    "/user"
}

pub fn repo_path(owner: &str, repo: &str) -> Result<String, String> {
    repo_base(owner, repo)
}

pub fn pull_path(owner: &str, repo: &str, n: i64) -> Result<String, String> {
    assert_pr_number(n)?;
    Ok(format!("{}/pulls/{n}", repo_base(owner, repo)?))
}

pub fn create_pull_path(owner: &str, repo: &str) -> Result<String, String> {
    Ok(format!("{}/pulls", repo_base(owner, repo)?))
}

pub fn requested_reviewers_path(owner: &str, repo: &str, n: i64) -> Result<String, String> {
    Ok(format!("{}/requested_reviewers", pull_path(owner, repo, n)?))
}

pub fn reply_path(owner: &str, repo: &str, n: i64, comment_id: i64) -> Result<String, String> {
    if comment_id < 1 {
        return Err("Not a comment id".into());
    }
    Ok(format!("{}/comments/{comment_id}/replies", pull_path(owner, repo, n)?))
}

/// Pull requests whose head is `branch` of the same repository, newest first.
pub fn branch_pulls_path(owner: &str, repo: &str, branch: &str, limit: i64) -> Result<String, String> {
    assert_branch(branch)?;
    let head = enc(&format!("{owner}:{branch}"));
    Ok(format!("{}/pulls?head={head}&state=all&sort=updated&direction=desc&per_page={}", repo_base(owner, repo)?, limit.clamp(1, 100)))
}

pub fn check_runs_path(owner: &str, repo: &str, sha: &str) -> Result<String, String> {
    if !SHA_RE.is_match(sha) {
        return Err("Not a commit hash".into());
    }
    Ok(format!("{}/commits/{sha}/check-runs?per_page=100", repo_base(owner, repo)?))
}

pub fn combined_status_path(owner: &str, repo: &str, sha: &str) -> Result<String, String> {
    if !SHA_RE.is_match(sha) {
        return Err("Not a commit hash".into());
    }
    Ok(format!("{}/commits/{sha}/status", repo_base(owner, repo)?))
}

/// The body of the create-pull-request call. Reviewers are added by a separate call afterwards.
pub fn create_pull_body(owner: &str, repo: &str, title: &str, description: Option<&str>, from_branch: &str, to_branch: &str) -> Result<Value, String> {
    assert_repo(owner, repo)?;
    assert_branch(from_branch)?;
    assert_branch(to_branch)?;
    Ok(json!({ "title": title, "body": description.unwrap_or(""), "head": from_branch, "base": to_branch }))
}

// ---- GraphQL ----

/// Review threads (with their comments) of one pull request. REST can't say whether a thread is resolved.
pub const REVIEW_THREADS_QUERY: &str = "query ReviewThreads($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          isResolved
          isOutdated
          path
          line
          originalLine
          diffSide
          comments(first: 100) {
            nodes {
              databaseId
              body
              createdAt
              author { login }
              replyTo { databaseId }
            }
          }
        }
      }
    }
  }
}";

const PULL_REQUEST_FIELDS: &str = "number title url updatedAt isDraft headRefName baseRefName headRefOid baseRefOid
  repository { name owner { login } }
  headRepository { name owner { login } }
  author { login }
  latestReviews(first: 20) { nodes { state author { login } } }
  reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } }";

/// Open pull requests matching a search (see `dashboard_search`).
pub fn dashboard_query() -> String {
    format!("query Dashboard($search: String!) {{\n  search(query: $search, type: ISSUE, first: 30) {{\n    nodes {{ ... on PullRequest {{ {PULL_REQUEST_FIELDS} }} }}\n  }}\n}}")
}

/// The search string for your open PRs (AUTHOR) or the ones waiting on your review (REVIEWER).
pub fn dashboard_search(role: &str, login: &str) -> Result<String, String> {
    if !LOGIN_RE.is_match(login) {
        return Err("Not a GitHub login".into());
    }
    let who = if role == "REVIEWER" { format!("review-requested:{login}") } else { format!("author:{login}") };
    Ok(format!("is:pr is:open archived:false {who}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bases(rest: &str, graphql: &str) -> ApiBases {
        ApiBases { rest: rest.into(), graphql: graphql.into() }
    }

    #[test]
    fn github_coms_api_is_api_github_com_an_enterprise_host_serves_rest_under_api_v3_and_graphql_under_api_graphql() {
        let dotcom = bases("https://api.github.com", "https://api.github.com/graphql");
        assert_eq!(api_bases("https://github.com").unwrap(), dotcom);
        assert_eq!(api_bases("https://GitHub.com").unwrap(), dotcom);
        assert_eq!(api_bases("https://www.github.com/").unwrap(), dotcom);
        assert_eq!(api_bases("https://ghe.example.com").unwrap(), bases("https://ghe.example.com/api/v3", "https://ghe.example.com/api/graphql"));
        assert_eq!(api_bases("https://ghe.example.com:8443").unwrap(), bases("https://ghe.example.com:8443/api/v3", "https://ghe.example.com:8443/api/graphql"));
        assert!(api_bases("not a url").is_err());
    }

    #[test]
    fn paths_are_pinned_to_the_repository_encode_their_segments_and_take_a_repo_like_dot_github() {
        assert_eq!(pull_path("octo", "hello", 7).unwrap(), "/repos/octo/hello/pulls/7");
        assert_eq!(pull_path("octo", ".github", 7).unwrap(), "/repos/octo/.github/pulls/7");
        assert_eq!(repo_path("octo", "hello").unwrap(), "/repos/octo/hello");
        assert_eq!(requested_reviewers_path("o", "r", 3).unwrap(), "/repos/o/r/pulls/3/requested_reviewers");
        assert_eq!(reply_path("o", "r", 3, 99).unwrap(), "/repos/o/r/pulls/3/comments/99/replies");
        assert_eq!(create_pull_path("o", "r").unwrap(), "/repos/o/r/pulls");
        assert_eq!(user_path(), "/user");
    }

    #[test]
    fn a_branchs_prs_are_asked_for_by_owner_branch_newest_first_capped() {
        assert_eq!(branch_pulls_path("octo", "hello", "feat/x", 10).unwrap(), "/repos/octo/hello/pulls?head=octo%3Afeat%2Fx&state=all&sort=updated&direction=desc&per_page=10");
        assert!(branch_pulls_path("octo", "hello", "b", 500).unwrap().ends_with("per_page=100"));
        assert!(branch_pulls_path("octo", "hello", "b", 0).unwrap().ends_with("per_page=1"));
    }

    #[test]
    fn a_commits_builds_are_two_reads_check_runs_and_the_combined_status() {
        let sha = "a".repeat(40);
        assert_eq!(check_runs_path("o", "r", &sha).unwrap(), format!("/repos/o/r/commits/{sha}/check-runs?per_page=100"));
        assert_eq!(combined_status_path("o", "r", &sha).unwrap(), format!("/repos/o/r/commits/{sha}/status"));
        assert!(check_runs_path("o", "r", &"A".repeat(7)).is_ok());
    }

    #[test]
    fn unsafe_owners_repos_numbers_branches_hashes_and_logins_are_refused_before_any_request() {
        assert!(pull_path("o", "../x", 1).unwrap_err().contains("Not a safe GitHub owner/repo"));
        assert!(pull_path("o/x", "r", 1).unwrap_err().contains("Not a safe GitHub owner/repo"));
        assert!(pull_path("o", "r", 0).unwrap_err().contains("Not a pull request number"));
        assert!(branch_pulls_path("o", "r", "-rf", 10).unwrap_err().contains("Not a safe branch name"));
        assert!(check_runs_path("o", "r", "zzz").unwrap_err().contains("Not a commit hash"));
        assert!(reply_path("o", "r", 1, 0).unwrap_err().contains("Not a comment id"));
        assert!(dashboard_search("AUTHOR", "a b").unwrap_err().contains("Not a GitHub login"));
        assert!(dashboard_search("AUTHOR", "x is:private").unwrap_err().contains("Not a GitHub login"));
        assert!(dashboard_search("AUTHOR", &"a".repeat(40)).is_err());
        assert!(dashboard_search("AUTHOR", "-a").is_err());
    }

    #[test]
    fn the_create_pr_body_names_the_branches_and_leaves_reviewers_to_the_follow_up_call() {
        assert_eq!(create_pull_body("o", "r", "T", Some("D"), "feat/x", "main").unwrap(), json!({ "title": "T", "body": "D", "head": "feat/x", "base": "main" }));
        assert_eq!(create_pull_body("o", "r", "T", None, "a", "b").unwrap()["body"], "");
        assert!(create_pull_body("o", "r", "T", Some(""), "--evil", "main").unwrap_err().contains("Not a safe branch name"));
    }

    #[test]
    fn the_dashboard_search_is_your_prs_or_the_ones_waiting_on_you_open_and_not_archived() {
        assert_eq!(dashboard_search("AUTHOR", "octo-cat").unwrap(), "is:pr is:open archived:false author:octo-cat");
        assert_eq!(dashboard_search("REVIEWER", "octo-cat").unwrap(), "is:pr is:open archived:false review-requested:octo-cat");
    }

    #[test]
    fn the_api_headers_ask_for_githubs_json_type_and_pin_a_version() {
        assert_eq!(API_HEADERS[0], ("Accept", "application/vnd.github+json"));
        assert!(Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(API_HEADERS[1].1));
    }

    #[test]
    fn the_dashboard_query_is_byte_identical_to_the_node_one() {
        assert_eq!(dashboard_query(), r#"query Dashboard($search: String!) {
  search(query: $search, type: ISSUE, first: 30) {
    nodes { ... on PullRequest { number title url updatedAt isDraft headRefName baseRefName headRefOid baseRefOid
  repository { name owner { login } }
  headRepository { name owner { login } }
  author { login }
  latestReviews(first: 20) { nodes { state author { login } } }
  reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } } } }
  }
}"#);
    }

    #[test]
    fn the_graphql_queries_ask_for_what_the_normalizers_read() {
        for f in ["isResolved", "isOutdated", "diffSide", "databaseId", "replyTo", "hasNextPage", "endCursor"] {
            assert!(REVIEW_THREADS_QUERY.contains(f), "{f}");
        }
        let q = dashboard_query();
        for f in ["headRefOid", "baseRefOid", "latestReviews", "reviewRequests", "headRepository"] {
            assert!(q.contains(f), "{f}");
        }
        assert!(q.starts_with("query Dashboard($search: String!) {\n  search(query: $search, type: ISSUE, first: 30) {\n    nodes { ... on PullRequest { number title"));
        assert!(q.ends_with("} } }\n  }\n}"));
    }
}
