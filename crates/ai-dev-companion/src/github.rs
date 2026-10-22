//! GitHub (github.com or Enterprise) as a git host over REST and GraphQL: the
//! counterpart of `bitbucket.rs` (port of `core/github.ts`), returning the same
//! shapes (`github_normalize` does the mapping) so features never learn which
//! host they are on. The API is reached with a personal access token only (the
//! browser's github.com session doesn't authenticate it), so every call goes
//! through a token-only `SiteAuth`; the web address (links, the extension's
//! page) is kept apart from the API address.

use crate::atlassian::{AtlassianError, AuthContext, Client, SiteAuth};
use crate::github_endpoints as endpoints;
use crate::github_normalize as normalize;
use crate::http::HttpTransport;
use crate::site_tokens::{normalize_base_url, SiteTokens};
use serde_json::{json, Value};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, AtlassianError>;

// Review threads are paged 100 at a time; five pages is the most one PR is asked for.
const MAX_THREAD_PAGES: usize = 5;

/// A GitHub site: its web address and the token-only sites its APIs are reached through.
#[derive(Debug, Clone)]
pub struct GithubSite {
    pub web: String,
    pub rest: SiteAuth,
    pub graphql: SiteAuth,
}

/// The GitHub site configured under `site_id` in config.json (the profile's git site).
pub fn github_site(config: &Value, tokens: &SiteTokens, site_id: &str) -> Result<GithubSite> {
    let default = crate::environment::current().default_base_url(site_id).to_string();
    let configured = config[site_id]["baseUrl"].as_str().filter(|b| !b.is_empty()).map(str::to_string);
    let raw = configured.or(Some(default).filter(|d| !d.is_empty())).unwrap_or_else(|| "https://github.com".into());
    let web = normalize_base_url(&raw);
    let config_key = format!("{site_id}.apiToken");
    let api_token = tokens.token(&config_key, &web);
    let bases = endpoints::api_bases(&web)?;
    let common = SiteAuth {
        label: "GitHub".into(),
        config_key,
        api_token,
        token_only: true,
        headers: endpoints::API_HEADERS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    Ok(GithubSite { rest: SiteAuth { base_url: bases.rest, ..common.clone() }, graphql: SiteAuth { base_url: bases.graphql, ..common }, web })
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("a normalized shape serializes")
}

/// What opening a PR needs; `owner` is the Bitbucket-style "project".
#[derive(Debug, Clone, Copy)]
pub struct PullRequestDraft<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub from_branch: &'a str,
    pub to_branch: &'a str,
    pub reviewers: &'a [String],
}

pub struct Github<'a, T: HttpTransport> {
    pub client: &'a Client<T>,
    pub site: GithubSite,
    /// The first wait before re-asking an unknown merge status (the second is twice this); 700 in production.
    pub merge_retry_ms: u64,
}

impl<'a, T: HttpTransport> Github<'a, T> {
    pub fn new(client: &'a Client<T>, site: GithubSite) -> Self {
        Self { client, site, merge_retry_ms: 700 }
    }

    async fn rest(&self, auth: &AuthContext, path: &str) -> Result<Value> {
        self.client.get(&self.site.rest, auth, path).await
    }

    /// `None` for a 404: the thing isn't there, which is not an error for optional lookups.
    async fn optional(&self, auth: &AuthContext, path: &str) -> Result<Option<Value>> {
        match self.rest(auth, path).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One GraphQL call. GitHub answers 200 with `errors` for a bad query or a missing repository.
    async fn graphql(&self, auth: &AuthContext, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let result = self.client.json(&self.site.graphql, auth, "", Some("POST"), Some(&body)).await?;
        let has_errors = result["errors"].as_array().is_some_and(|e| !e.is_empty());
        let has_data = crate::js_text::js_truthy(result.get("data"));
        if has_errors && !has_data {
            let message = result["errors"][0]["message"].as_str().filter(|m| !m.is_empty()).unwrap_or("unknown error");
            return Err(AtlassianError::Status { message: format!("GitHub GraphQL request failed: {message}"), status: 200 });
        }
        Ok(result)
    }

    /// The signed-in user's login, lower-cased; what comments' and reviewers' names are compared with.
    pub async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>> {
        let user = self.rest(auth, endpoints::user_path()).await?;
        Ok(user["login"].as_str().filter(|l| !l.is_empty()).map(str::to_lowercase))
    }

    pub async fn get_pull_request(&self, auth: &AuthContext, owner: &str, repo: &str, n: i64) -> Result<Option<Value>> {
        Ok(normalize::normalize_pull_request(&self.rest(auth, &endpoints::pull_path(owner, repo, n)?).await?).map(|p| to_value(&p)))
    }

    pub async fn list_activities(&self, auth: &AuthContext, owner: &str, repo: &str, n: i64) -> Result<Vec<Value>> {
        endpoints::pull_path(owner, repo, n)?; // validates the arguments before any request
        let mut pages = Vec::new();
        let mut cursor = Value::Null;
        for page in 0..MAX_THREAD_PAGES {
            let result = self.graphql(auth, endpoints::REVIEW_THREADS_QUERY, json!({ "owner": owner, "name": repo, "number": n, "cursor": cursor })).await?;
            let info = result["data"]["repository"]["pullRequest"]["reviewThreads"]["pageInfo"].clone();
            pages.push(result);
            let end = info["endCursor"].as_str().filter(|c| !c.is_empty());
            let (true, Some(end)) = (info["hasNextPage"] == json!(true), end) else { break };
            cursor = json!(end);
            if page == MAX_THREAD_PAGES - 1 {
                eprintln!("[github] {owner}/{repo}#{n} has more than {} review threads; the rest are not read.", MAX_THREAD_PAGES * 100);
            }
        }
        Ok(normalize::normalize_review_threads(&Value::Array(pages)).iter().map(to_value).collect())
    }

    /// Replies in the thread of review comment `parent_id`. Returns the new comment's id, or `None`.
    pub async fn reply_to_comment(&self, auth: &AuthContext, owner: &str, repo: &str, n: i64, parent_id: i64, text: &str) -> Result<Option<i64>> {
        let created = self.client.json(&self.site.rest, auth, &endpoints::reply_path(owner, repo, n, parent_id)?, Some("POST"), Some(&json!({ "body": text }))).await?;
        Ok(created["id"].as_i64())
    }

    pub async fn get_default_branch(&self, auth: &AuthContext, owner: &str, repo: &str) -> Result<Option<String>> {
        Ok(self.optional(auth, &endpoints::repo_path(owner, repo)?).await?.and_then(|r| normalize::normalize_default_branch(&r)))
    }

    pub async fn list_branch_pull_requests(&self, auth: &AuthContext, owner: &str, repo: &str, branch: &str) -> Result<Vec<Value>> {
        let path = endpoints::branch_pulls_path(owner, repo, branch, 10)?;
        Ok(normalize::normalize_pull_request_list(&self.rest(auth, &path).await?).iter().map(to_value).collect())
    }

    pub async fn create_pull_request(&self, auth: &AuthContext, d: &PullRequestDraft<'_>) -> Result<Value> {
        let PullRequestDraft { owner, repo, title, description, from_branch, to_branch, reviewers } = *d;
        let body = endpoints::create_pull_body(owner, repo, title, description, from_branch, to_branch)?;
        let created = self.client.json(&self.site.rest, auth, &endpoints::create_pull_path(owner, repo)?, Some("POST"), Some(&body)).await?;
        let summary = normalize::normalize_pull_request_summary(&created).ok_or_else(|| AtlassianError::Other("GitHub didn't return the new pull request.".into()))?;
        // Reviewers are a separate call on GitHub. The PR exists either way, so a refused
        // reviewer (no access, or the author themselves) doesn't fail the create.
        if !reviewers.is_empty() {
            let id = summary.id.as_i64().unwrap_or(0);
            let path = endpoints::requested_reviewers_path(owner, repo, id)?;
            if let Err(e) = self.client.json(&self.site.rest, auth, &path, Some("POST"), Some(&json!({ "reviewers": reviewers }))).await {
                eprintln!("[github] couldn't request reviewers on {owner}/{repo}#{id}: {e}");
            }
        }
        Ok(to_value(&summary))
    }

    pub async fn get_repository_id(&self, auth: &AuthContext, owner: &str, repo: &str) -> Result<i64> {
        let found = normalize::normalize_repository(&self.rest(auth, &endpoints::repo_path(owner, repo)?).await?);
        found.and_then(|r| r.id.as_i64()).ok_or_else(|| AtlassianError::Other("GitHub didn't return the repository.".into()))
    }

    /// GitHub has no default reviewers (CODEOWNERS requests them itself when the PR opens).
    pub async fn get_default_reviewers(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// The commit's check runs and commit statuses together; `None` when it has none.
    pub async fn get_commit_build_status(&self, auth: &AuthContext, owner: &str, repo: &str, sha: &str) -> Result<Option<Value>> {
        let check_runs = self.optional(auth, &endpoints::check_runs_path(owner, repo, sha)?).await?.unwrap_or(Value::Null);
        let status = self.optional(auth, &endpoints::combined_status_path(owner, repo, sha)?).await?.unwrap_or(Value::Null);
        Ok(normalize::normalize_build_status(&check_runs, &status).map(|b| to_value(&b)))
    }

    /// Your open PRs (`AUTHOR`), or the ones waiting on your review (`REVIEWER`).
    pub async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>> {
        let Some(me) = self.who_am_i(auth).await? else { return Ok(vec![]) };
        let search = endpoints::dashboard_search(role, &me)?;
        let result = self.graphql(auth, &endpoints::dashboard_query(), json!({ "search": search })).await?;
        Ok(normalize::normalize_dashboard(&result).iter().map(to_value).collect())
    }

    /// Whether the PR conflicts with its target. GitHub works this out lazily: the first read can
    /// answer "not known yet" (null), so an unknown answer is asked again a couple of times, and
    /// stays unknown rather than being read as "no conflict".
    pub async fn get_merge_status(&self, auth: &AuthContext, owner: &str, repo: &str, n: i64) -> Result<Value> {
        let path = endpoints::pull_path(owner, repo, n)?;
        let mut status = normalize::normalize_merge_status(&self.rest(auth, &path).await?);
        for attempt in 0..2u64 {
            if status.conflicted.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(self.merge_retry_ms * (attempt + 1))).await;
            status = normalize::normalize_merge_status(&self.rest(auth, &path).await?);
        }
        Ok(to_value(&status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_context::AuthHub;
    use crate::http::testing::FakeTransport;
    use crate::http::{HttpResponse, TransportError};
    use crate::site_tokens::testing::{tokens, Fixed};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    type Answer = std::result::Result<HttpResponse, TransportError>;

    fn status(code: u16, body: Value) -> Answer {
        Ok(HttpResponse { status: code, headers: vec![], body: body.to_string().into_bytes() })
    }
    fn ok(body: Value) -> Answer {
        status(200, body)
    }

    fn client(answers: Vec<Answer>) -> Client<FakeTransport> {
        let q = Mutex::new(VecDeque::from(answers));
        let t = FakeTransport::new(move |_| q.lock().unwrap().pop_front().expect("more calls than answers"));
        Client::new(t, Arc::new(AuthHub::new(Arc::new(Fixed(vec![])))))
    }

    fn gh(c: &Client<FakeTransport>) -> Github<'_, FakeTransport> {
        let site = github_site(&json!({}), &tokens(vec![("github.apiToken", "ghp_x")]), "github").unwrap();
        let mut g = Github::new(c, site);
        g.merge_retry_ms = 0;
        g
    }

    fn urls(c: &Client<FakeTransport>) -> Vec<String> {
        c.transport.seen.lock().unwrap().iter().map(|r| r.url.clone()).collect()
    }
    fn none() -> AuthContext {
        AuthContext::default()
    }

    fn raw_pr(mergeable: Value) -> Value {
        json!({ "number": 5, "title": "T", "state": "open", "mergeable": mergeable, "mergeable_state": "unknown", "html_url": "https://github.com/o/r/pull/5", "head": { "ref": "feat", "sha": "abc" }, "base": { "ref": "main" }, "updated_at": "2026-01-01T00:00:00Z" })
    }

    #[test]
    fn the_site_is_token_only_with_api_headers_and_separate_rest_and_graphql_bases() {
        let s = github_site(&json!({}), &tokens(vec![("github.apiToken", "t")]), "github").unwrap();
        assert_eq!((s.web.as_str(), s.rest.base_url.as_str(), s.graphql.base_url.as_str()), ("https://github.com", "https://api.github.com", "https://api.github.com/graphql"));
        assert!(s.rest.token_only && s.graphql.token_only);
        assert_eq!(s.rest.api_token.as_deref(), Some("t"));
        assert!(s.rest.headers.iter().any(|(k, v)| k == "Accept" && v == "application/vnd.github+json"));
        let e = github_site(&json!({ "github": { "baseUrl": "ghe.example.com/" } }), &tokens(vec![]), "github").unwrap();
        assert_eq!((e.web.as_str(), e.rest.base_url.as_str(), e.rest.api_token.clone()), ("https://ghe.example.com", "https://ghe.example.com/api/v3", None));
    }

    #[tokio::test]
    async fn who_am_i_lowercases_the_login_and_a_pr_is_fetched_and_normalized() {
        let c = client(vec![ok(json!({ "login": "JDoe" })), ok(json!({})), ok(raw_pr(json!(true)))]);
        let g = gh(&c);
        assert_eq!(g.who_am_i(&none()).await.unwrap().as_deref(), Some("jdoe"));
        assert_eq!(g.who_am_i(&none()).await.unwrap(), None);
        let pr = g.get_pull_request(&none(), "o", "r", 5).await.unwrap().unwrap();
        assert_eq!((pr["id"].clone(), pr["state"].clone(), pr["fromBranch"].clone()), (json!(5), json!("OPEN"), json!("feat")));
        assert_eq!(urls(&c), ["https://api.github.com/user", "https://api.github.com/user", "https://api.github.com/repos/o/r/pulls/5"]);
    }

    #[tokio::test]
    async fn graphql_errors_without_data_fail_and_a_partial_answer_passes() {
        let c = client(vec![ok(json!({ "errors": [{ "message": "Could not resolve" }] })), ok(json!({ "errors": [{ "message": "x" }], "data": { "repository": null } }))]);
        let g = gh(&c);
        let e = g.list_activities(&none(), "o", "r", 5).await.unwrap_err();
        assert_eq!((e.to_string().as_str(), e.status()), ("GitHub GraphQL request failed: Could not resolve", Some(200)));
        assert!(g.list_activities(&none(), "o", "r", 5).await.unwrap().is_empty());
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].url.as_str()), ("POST", "https://api.github.com/graphql"));
        let sent: Value = serde_json::from_slice(seen[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["variables"], json!({ "owner": "o", "name": "r", "number": 5, "cursor": null }));
    }

    #[tokio::test]
    async fn review_threads_page_by_cursor_and_stop_at_five_pages() {
        let page = |next: bool| ok(json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "pageInfo": { "hasNextPage": next, "endCursor": "c1" }, "nodes": [] } } } } }));
        let c = client(vec![page(true), page(false)]);
        gh(&c).list_activities(&none(), "o", "r", 5).await.unwrap();
        assert_eq!(c.transport.seen.lock().unwrap().len(), 2);
        let second: Value = serde_json::from_slice(c.transport.seen.lock().unwrap()[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(second["variables"]["cursor"], "c1");
        let endless = client((0..6).map(|_| page(true)).collect());
        gh(&endless).list_activities(&none(), "o", "r", 5).await.unwrap();
        assert_eq!(endless.transport.seen.lock().unwrap().len(), MAX_THREAD_PAGES);
    }

    #[tokio::test]
    async fn arguments_are_validated_before_any_request() {
        let c = client(vec![]);
        let g = gh(&c);
        assert!(g.list_activities(&none(), "../x", "r", 5).await.is_err());
        assert!(g.get_pull_request(&none(), "o", "r/../x", 5).await.is_err());
        assert!(c.transport.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn replying_returns_a_numeric_id_and_the_default_branch_is_none_on_404() {
        let c = client(vec![ok(json!({ "id": 42 })), ok(json!({ "id": "x" })), status(404, json!({})), ok(json!({ "default_branch": "main" })), status(500, json!({ "message": "boom" }))]);
        let g = gh(&c);
        assert_eq!(g.reply_to_comment(&none(), "o", "r", 5, 9, "hi").await.unwrap(), Some(42));
        assert_eq!(g.reply_to_comment(&none(), "o", "r", 5, 9, "hi").await.unwrap(), None);
        assert_eq!(g.get_default_branch(&none(), "o", "r").await.unwrap(), None);
        assert_eq!(g.get_default_branch(&none(), "o", "r").await.unwrap().as_deref(), Some("main"));
        assert_eq!(g.get_default_branch(&none(), "o", "r").await.unwrap_err().status(), Some(500));
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(seen[0].body.as_deref().unwrap()).unwrap(), json!({ "body": "hi" }));
    }

    #[tokio::test]
    async fn creating_a_pr_requests_reviewers_separately_and_a_refusal_only_warns() {
        let created = json!({ "number": 8, "title": "T", "state": "open", "html_url": "u", "head": { "ref": "b" }, "base": { "ref": "main" } });
        let c = client(vec![ok(created.clone()), ok(json!({})), ok(created), status(422, json!({ "message": "no" })), ok(json!({}))]);
        let g = gh(&c);
        let r = vec!["ann".to_string()];
        let d = |description, reviewers| PullRequestDraft { owner: "o", repo: "r", title: "T", description, from_branch: "b", to_branch: "main", reviewers };
        assert_eq!(g.create_pull_request(&none(), &d(Some("D"), &r)).await.unwrap()["id"], 8);
        assert_eq!(g.create_pull_request(&none(), &d(None, &r)).await.unwrap()["id"], 8, "a refused reviewer doesn't fail the create");
        assert!(g.create_pull_request(&none(), &d(None, &[])).await.unwrap_err().to_string().contains("didn't return the new pull request"));
        let u = urls(&c);
        assert_eq!(u[1], "https://api.github.com/repos/o/r/pulls/8/requested_reviewers");
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(seen[1].body.as_deref().unwrap()).unwrap(), json!({ "reviewers": ["ann"] }));
        assert_eq!(serde_json::from_slice::<Value>(seen[0].body.as_deref().unwrap()).unwrap()["head"], "b");
    }

    #[tokio::test]
    async fn repository_ids_default_reviewers_and_build_status_with_missing_parts() {
        let c = client(vec![ok(json!({ "id": 99, "name": "r", "owner": { "login": "o" } })), ok(json!({})), status(404, json!({})), status(404, json!({}))]);
        let g = gh(&c);
        assert_eq!(g.get_repository_id(&none(), "o", "r").await.unwrap(), 99);
        assert!(g.get_repository_id(&none(), "o", "r").await.unwrap_err().to_string().contains("didn't return the repository"));
        assert!(g.get_default_reviewers().await.unwrap().is_empty());
        assert_eq!(g.get_commit_build_status(&none(), "o", "r", &"a".repeat(40)).await.unwrap(), None);
        assert_eq!(urls(&c)[2], format!("https://api.github.com/repos/o/r/commits/{}/check-runs?per_page=100", "a".repeat(40)));
    }

    #[tokio::test]
    async fn the_dashboard_needs_a_login_and_searches_by_role() {
        let c = client(vec![ok(json!({})), ok(json!({ "login": "JDoe" })), ok(json!({ "data": { "search": { "nodes": [] } } }))]);
        let g = gh(&c);
        assert!(g.list_dashboard_pull_requests(&none(), "AUTHOR").await.unwrap().is_empty());
        assert!(g.list_dashboard_pull_requests(&none(), "REVIEWER").await.unwrap().is_empty());
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let sent: Value = serde_json::from_slice(seen[2].body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["variables"]["search"], "is:pr is:open archived:false review-requested:jdoe");
    }

    #[tokio::test]
    async fn an_unknown_merge_status_is_asked_again_twice_and_then_stays_unknown() {
        let c = client(vec![ok(raw_pr(Value::Null)), ok(raw_pr(Value::Null)), ok(raw_pr(json!(false)))]);
        assert_eq!(gh(&c).get_merge_status(&none(), "o", "r", 5).await.unwrap()["conflicted"], true);
        assert_eq!(c.transport.seen.lock().unwrap().len(), 3);
        let never = client(vec![ok(raw_pr(Value::Null)), ok(raw_pr(Value::Null)), ok(raw_pr(Value::Null))]);
        assert_eq!(gh(&never).get_merge_status(&none(), "o", "r", 5).await.unwrap()["conflicted"], Value::Null);
        let known = client(vec![ok(raw_pr(json!(true)))]);
        assert_eq!(gh(&known).get_merge_status(&none(), "o", "r", 5).await.unwrap()["conflicted"], false);
        assert_eq!(known.transport.seen.lock().unwrap().len(), 1);
    }
}
