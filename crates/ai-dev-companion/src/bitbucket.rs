//! The "talk to Bitbucket Server/DC" client (port of `core/bitbucket.ts`),
//! mirroring the Jenkins one. Auth reuses `atlassian` wholesale: the relayed
//! browser session first, a configured API token as fallback, `AuthSetup`
//! otherwise, so there's exactly one copy of that fallback chain. Bitbucket
//! takes a Bearer token (no username needed).
//!
//! Everything goes through `/rest/api/1.0/` and returns the normalized shapes
//! from `bitbucket_normalize`, as the JSON the Node service served. This file's
//! own job is the HTTP plumbing: building paths, following activities'
//! pagination (capped) and reading the `X-AUSERNAME` response header.
//!
//! Doctor's contract check needs the raw responses too, to diff their keys, so
//! the raw fetchers are exported alongside the normalized ones.

use crate::atlassian::{AtlassianError, AuthContext, Client, SiteAuth};
use crate::bitbucket_contract::{follow_pages, Pages};
use crate::bitbucket_endpoints as endpoints;
use crate::bitbucket_normalize as normalize;
use crate::environment;
use crate::http::HttpTransport;
use crate::site_tokens::{normalize_base_url, SiteTokens};
use crate::uri::encode_uri_component;
use serde_json::{json, Value};

const API_ROOT: &str = "/rest/api/1.0";
// A PR with an unusually long activity history shouldn't turn "collect its
// review comments" into an unbounded fetch: hitting the cap warns, never fails.
const MAX_ACTIVITY_PAGES: usize = 20;

pub type Result<T> = std::result::Result<T, AtlassianError>;

/// Bitbucket as an authenticated site: Bearer token (the default scheme).
pub fn bitbucket_site(config: &Value, tokens: &SiteTokens) -> SiteAuth {
    let default = environment::current().default_base_url("bitbucket").to_string();
    let base_url = normalize_base_url(config["bitbucket"]["baseUrl"].as_str().filter(|b| !b.is_empty()).unwrap_or(&default));
    // Own saved token first, then the one another tool already stored for this host.
    let api_token = tokens.token("bitbucket.apiToken", &base_url);
    SiteAuth { base_url, label: "Bitbucket".into(), config_key: "bitbucket.apiToken".into(), api_token, ..Default::default() }
}

fn pr_path(project: &str, repo: &str, pr_id: i64, suffix: &str) -> String {
    format!("{API_ROOT}/projects/{}/repos/{}/pull-requests/{pr_id}{suffix}", encode_uri_component(project), encode_uri_component(repo))
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("a normalized shape serializes")
}

/// One Bitbucket site, bound to a client and the request's auth.
pub struct Bitbucket<'a, T: HttpTransport> {
    pub client: &'a Client<T>,
    pub site: SiteAuth,
}

impl<T: HttpTransport> Bitbucket<'_, T> {
    /// `GET .../pull-requests/{id}`, unnormalized: for doctor's live contract check.
    pub async fn fetch_pull_request_raw(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Value> {
        self.client.get(&self.site, auth, &pr_path(project, repo, pr_id, "")).await
    }

    /// The normalized PR.
    pub async fn get_pull_request(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Option<Value>> {
        Ok(normalize::normalize_pull_request(&self.fetch_pull_request_raw(auth, project, repo, pr_id).await?).map(|p| to_value(&p)))
    }

    /// One page of `GET .../pull-requests/{id}/activities`, unnormalized.
    pub async fn fetch_activity_page(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64, start: i64) -> Result<Value> {
        let query = if start != 0 { format!("?start={start}") } else { String::new() };
        self.client.get(&self.site, auth, &pr_path(project, repo, pr_id, &format!("/activities{query}"))).await
    }

    /// Every activities page for this PR, raw: follows `nextPageStart` until `isLastPage`, capped.
    pub async fn fetch_all_activity_pages(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Pages> {
        follow_pages(|start| self.fetch_activity_page(auth, project, repo, pr_id, start), MAX_ACTIVITY_PAGES, |m| eprintln!("{m}")).await
    }

    /// Every review comment on this PR, normalized.
    pub async fn list_activities(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Vec<Value>> {
        let Pages { pages, truncated, .. } = self.fetch_all_activity_pages(auth, project, repo, pr_id).await?;
        if truncated {
            eprintln!("[bitbucket] Hit the {MAX_ACTIVITY_PAGES}-page cap fetching activities for {project}/{repo}#{pr_id} \u{2014} some older comments may be missing.");
        }
        Ok(normalize::normalize_activities(&Value::Array(pages)).iter().map(to_value).collect())
    }

    /// `GET /application-properties`, unnormalized: for doctor's contract check and version tracking.
    pub async fn fetch_application_properties_raw(&self, auth: &AuthContext) -> Result<Value> {
        self.client.get(&self.site, auth, &format!("{API_ROOT}/application-properties")).await
    }

    pub async fn application_properties(&self, auth: &AuthContext) -> Result<Option<Value>> {
        Ok(normalize::normalize_app_properties(&self.fetch_application_properties_raw(auth).await?).map(|p| to_value(&p)))
    }

    /// The logged-in Bitbucket username, from the `X-AUSERNAME` response header of
    /// a cheap authenticated call. `None` (not an error) when the header is absent.
    pub async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>> {
        let r = self.client.json_with_headers(&self.site, auth, &format!("{API_ROOT}/application-properties"), None, None).await?;
        Ok(r.headers.iter().find(|(k, _)| k == "x-ausername").map(|(_, v)| v.clone()).filter(|v| !v.is_empty()))
    }

    /// Posts a reply under `parent_id` and returns the created comment's id, or
    /// `None` if the response doesn't carry a numeric one.
    pub async fn reply_to_comment(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64, parent_id: i64, text: &str) -> Result<Option<i64>> {
        let body = json!({ "text": text, "parent": { "id": parent_id } });
        let created = self.client.json(&self.site, auth, &pr_path(project, repo, pr_id, "/comments"), Some("POST"), Some(&body)).await?;
        Ok(created["id"].as_i64())
    }

    /// The repo's default branch per Bitbucket (clones often have no origin/HEAD),
    /// or `None` when neither endpoint exists on this version.
    pub async fn get_default_branch(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<Option<String>> {
        let paths = endpoints::default_branch_paths(project, repo)?;
        match endpoints::first_available(&paths, |p| async move { self.client.get(&self.site, auth, &p).await }, AtlassianError::is_not_found).await {
            Ok((_, body)) => Ok(normalize::normalize_default_branch(&body)),
            Err(endpoints::FirstAvailableError::Failed(e)) if e.is_not_found() => Ok(None),
            Err(endpoints::FirstAvailableError::Failed(e)) => Err(e),
            Err(endpoints::FirstAvailableError::NoEndpoint) => Err(AtlassianError::Other("No endpoint to try.".into())),
        }
    }

    /// PRs (any state, newest first) whose source branch is `branch`.
    pub async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>> {
        let path = endpoints::branch_pull_requests_path(project, repo, branch, "ALL", 10)?;
        Ok(normalize::normalize_pull_request_list(&self.client.get(&self.site, auth, &path).await?).iter().map(to_value).collect())
    }

    /// Build statuses of a commit, from the build-status plugin. `None` when the endpoint doesn't exist.
    pub async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>> {
        let _ = (project, repo);
        let paths = endpoints::commit_builds_paths(sha)?;
        match endpoints::first_available(&paths, |p| async move { self.client.get(&self.site, auth, &p).await }, AtlassianError::is_not_found).await {
            Ok((_, body)) => Ok(Some(to_value(&normalize::normalize_build_statuses(&body)))),
            Err(endpoints::FirstAvailableError::Failed(e)) if e.is_not_found() => Ok(None),
            Err(endpoints::FirstAvailableError::Failed(e)) => Err(e),
            Err(endpoints::FirstAvailableError::NoEndpoint) => Err(AtlassianError::Other("No endpoint to try.".into())),
        }
    }

    pub async fn get_repository_id(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<i64> {
        let raw = self.client.get(&self.site, auth, &endpoints::repo_path(project, repo)?).await?;
        normalize::normalize_repository(&raw).and_then(|r| r.id.as_i64()).ok_or_else(|| AtlassianError::Other(format!("Bitbucket didn't return an id for {project}/{repo}.")))
    }

    /// Usernames the repo's default-reviewer conditions pick for this branch pair.
    pub async fn get_default_reviewers(&self, auth: &AuthContext, project: &str, repo: &str, q: &endpoints::DefaultReviewersQuery<'_>) -> Result<Vec<String>> {
        let path = endpoints::default_reviewers_path(project, repo, q)?;
        Ok(normalize::normalize_default_reviewers(&self.client.get(&self.site, auth, &path).await?))
    }

    /// Your open PRs (`AUTHOR`), or the ones waiting on your review (`REVIEWER`, unapproved).
    pub async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>> {
        let status = (role == "REVIEWER").then_some("UNAPPROVED");
        let path = endpoints::dashboard_pull_requests_path(role, status, 50)?;
        Ok(normalize::normalize_dashboard_pull_requests(&self.client.get(&self.site, auth, &path).await?).iter().map(to_value).collect())
    }

    pub async fn get_merge_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Value> {
        let path = endpoints::merge_status_path(project, repo, pr_id)?;
        Ok(to_value(&normalize::normalize_merge_status(&self.client.get(&self.site, auth, &path).await?)))
    }

    /// Opens a same-repo PR. Returns it normalized (with its page URL).
    pub async fn create_pull_request(&self, auth: &AuthContext, d: &endpoints::PullRequestDraft<'_>) -> Result<Value> {
        let body = endpoints::create_pull_request_body(d)?;
        let created = self.client.json(&self.site, auth, &endpoints::create_pull_request_path(d.project, d.repo)?, Some("POST"), Some(&body)).await?;
        normalize::normalize_pull_request_summary(&created).filter(|p| p.base.id.is_some()).map(|p| to_value(&p)).ok_or_else(|| AtlassianError::Other("Bitbucket didn't return the new pull request.".into()))
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

    fn ok(body: Value) -> std::result::Result<HttpResponse, TransportError> {
        Ok(HttpResponse { status: 200, headers: vec![("x-ausername".into(), "jdoe".into())], body: body.to_string().into_bytes() })
    }

    fn status(code: u16, body: Value) -> std::result::Result<HttpResponse, TransportError> {
        Ok(HttpResponse { status: code, headers: vec![], body: body.to_string().into_bytes() })
    }

    fn client(answers: Vec<std::result::Result<HttpResponse, TransportError>>) -> Client<FakeTransport> {
        let q = Mutex::new(VecDeque::from(answers));
        let t = FakeTransport::new(move |_| q.lock().unwrap().pop_front().expect("more calls than answers"));
        Client::new(t, Arc::new(AuthHub::new(Arc::new(Fixed(vec![])))))
    }

    fn bb(c: &Client<FakeTransport>) -> Bitbucket<'_, FakeTransport> {
        let site = SiteAuth { base_url: "https://bb.example.com".into(), label: "Bitbucket".into(), config_key: "bitbucket.apiToken".into(), api_token: Some("tok".into()), ..Default::default() };
        Bitbucket { client: c, site }
    }

    fn urls(c: &Client<FakeTransport>) -> Vec<String> {
        c.transport.seen.lock().unwrap().iter().map(|r| r.url.replace("https://bb.example.com", "")).collect()
    }

    fn none() -> AuthContext {
        AuthContext::default()
    }

    fn raw_pr() -> Value {
        json!({ "id": 12, "title": "Fix", "state": "OPEN", "fromRef": { "id": "refs/heads/f", "displayId": "f", "latestCommit": "aaa", "repository": { "slug": "app", "project": { "key": "ACME" } } }, "toRef": { "id": "refs/heads/master", "latestCommit": "bbb", "repository": { "slug": "app", "project": { "key": "ACME" } } }, "properties": { "commentCount": 3 } })
    }

    #[test]
    fn the_site_uses_the_configured_base_url_normalized_and_the_saved_token() {
        let t = tokens(vec![("bitbucket.apiToken", "own")]);
        let s = bitbucket_site(&json!({ "bitbucket": { "baseUrl": "bb.example.com/" } }), &t);
        assert_eq!((s.base_url.as_str(), s.label.as_str(), s.config_key.as_str(), s.api_token.as_deref()), ("https://bb.example.com", "Bitbucket", "bitbucket.apiToken", Some("own")));
        assert_eq!(bitbucket_site(&json!({}), &tokens(vec![])).api_token, None);
    }

    #[tokio::test]
    async fn a_pull_request_is_fetched_by_its_encoded_path_and_normalized() {
        let c = client(vec![ok(raw_pr())]);
        let pr = bb(&c).get_pull_request(&none(), "ACME", "my repo", 12).await.unwrap().unwrap();
        assert_eq!(urls(&c), ["/rest/api/1.0/projects/ACME/repos/my%20repo/pull-requests/12"]);
        assert_eq!((pr["id"].clone(), pr["state"].clone(), pr["fromBranch"].clone()), (json!(12), json!("OPEN"), json!("f")));
    }

    #[tokio::test]
    async fn activities_follow_the_next_page_start_and_a_comment_per_value_is_normalized() {
        let comment = |id: i64| json!({ "action": "COMMENTED", "comment": { "id": id, "text": "t", "author": { "slug": "a" }, "createdDate": 1, "updatedDate": 1, "state": "OPEN", "comments": [] }, "commentAnchor": { "path": "a.ts", "line": 3, "lineType": "ADDED" } });
        let c = client(vec![ok(json!({ "values": [comment(1)], "isLastPage": false, "nextPageStart": 25 })), ok(json!({ "values": [comment(2)], "isLastPage": true }))]);
        let comments = bb(&c).list_activities(&none(), "ACME", "app", 7).await.unwrap();
        assert_eq!(urls(&c), ["/rest/api/1.0/projects/ACME/repos/app/pull-requests/7/activities", "/rest/api/1.0/projects/ACME/repos/app/pull-requests/7/activities?start=25"]);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["id"], 1);
    }

    #[tokio::test]
    async fn who_am_i_reads_the_header_and_a_missing_one_is_none_not_an_error() {
        let c = client(vec![ok(json!({ "version": "9.4.16" })), status(200, json!({}))]);
        assert_eq!(bb(&c).who_am_i(&none()).await.unwrap().as_deref(), Some("jdoe"));
        assert_eq!(bb(&c).who_am_i(&none()).await.unwrap(), None);
        assert_eq!(urls(&c)[0], "/rest/api/1.0/application-properties");
    }

    #[tokio::test]
    async fn replying_posts_the_text_under_the_parent_and_returns_the_new_id_when_numeric() {
        let c = client(vec![ok(json!({ "id": 99 })), ok(json!({ "id": "x" }))]);
        assert_eq!(bb(&c).reply_to_comment(&none(), "ACME", "app", 7, 5, "thanks").await.unwrap(), Some(99));
        assert_eq!(bb(&c).reply_to_comment(&none(), "ACME", "app", 7, 5, "again").await.unwrap(), None);
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!(seen[0].method, "POST");
        assert_eq!(serde_json::from_slice::<Value>(seen[0].body.as_deref().unwrap()).unwrap(), json!({ "text": "thanks", "parent": { "id": 5 } }));
        assert!(seen[0].url.ends_with("/pull-requests/7/comments"));
    }

    #[tokio::test]
    async fn the_default_branch_falls_back_to_the_old_endpoint_on_404_and_is_none_when_neither_exists() {
        let c = client(vec![status(404, json!({})), ok(json!({ "displayId": "main", "id": "refs/heads/main" }))]);
        assert_eq!(bb(&c).get_default_branch(&none(), "ACME", "app").await.unwrap().as_deref(), Some("main"));
        assert_eq!(urls(&c), ["/rest/api/1.0/projects/ACME/repos/app/default-branch", "/rest/api/1.0/projects/ACME/repos/app/branches/default"]);
        let gone = client(vec![status(404, json!({})), status(404, json!({}))]);
        assert_eq!(bb(&gone).get_default_branch(&none(), "ACME", "app").await.unwrap(), None);
        let denied = client(vec![status(500, json!({ "errorMessages": ["boom"] }))]);
        assert_eq!(bb(&denied).get_default_branch(&none(), "ACME", "app").await.unwrap_err().status(), Some(500));
        assert!(bb(&gone).get_default_branch(&none(), "..", "app").await.unwrap_err().to_string().starts_with("Not a safe Bitbucket project/repo"));
    }

    #[tokio::test]
    async fn branch_prs_commit_builds_and_repository_ids_use_their_endpoints() {
        let pr = json!({ "id": 3, "title": "T", "state": "OPEN", "fromRef": { "id": "refs/heads/b", "latestCommit": "aaa" }, "toRef": { "id": "refs/heads/master" }, "links": { "self": [{ "href": "https://bb.example.com/projects/ACME/repos/app/pull-requests/3" }] } });
        let c = client(vec![ok(json!({ "values": [pr] })), ok(json!({ "values": [{ "state": "SUCCESSFUL", "key": "k", "name": "n", "url": "u", "dateAdded": 5 }] })), status(404, json!({})), ok(json!({ "id": 77, "slug": "app", "project": { "key": "ACME" } })), ok(json!({}))]);
        let b = bb(&c);
        let prs = b.list_branch_pull_requests(&none(), "ACME", "app", "bugfix/x").await.unwrap();
        assert_eq!(prs[0]["id"], 3);
        let sha = "a".repeat(40);
        assert_eq!(b.get_commit_build_status(&none(), "ACME", "app", &sha).await.unwrap().unwrap()["state"], "SUCCESSFUL");
        assert_eq!(b.get_commit_build_status(&none(), "ACME", "app", &sha).await.unwrap(), None, "no build-status plugin");
        assert_eq!(b.get_repository_id(&none(), "ACME", "app").await.unwrap(), 77);
        assert!(b.get_repository_id(&none(), "ACME", "app").await.unwrap_err().to_string().contains("didn't return an id for ACME/app"));
        let u = urls(&c);
        assert!(u[0].contains("/pull-requests?state=ALL&direction=OUTGOING&at=refs%2Fheads%2Fbugfix%2Fx"), "{}", u[0]);
        assert_eq!(u[1], format!("/rest/build-status/1.0/commits/{sha}"));
    }

    #[tokio::test]
    async fn the_dashboard_and_merge_status_endpoints_and_pr_creation() {
        let c = client(vec![ok(json!({ "values": [] })), ok(json!({ "values": [] })), ok(json!({ "canMerge": false, "conflicted": true, "outcome": "CONFLICTED" })), ok(json!({ "id": 31, "title": "T", "state": "OPEN", "fromRef": { "id": "refs/heads/b" }, "toRef": { "id": "refs/heads/master" } })), status(200, json!({}))]);
        let b = bb(&c);
        b.list_dashboard_pull_requests(&none(), "AUTHOR").await.unwrap();
        b.list_dashboard_pull_requests(&none(), "REVIEWER").await.unwrap();
        assert_eq!(b.get_merge_status(&none(), "ACME", "app", 4).await.unwrap()["conflicted"], true);
        let reviewers = vec!["ann".to_string()];
        let draft = endpoints::PullRequestDraft { project: "ACME", repo: "app", title: &json!("T"), description: &json!("D"), from_branch: "b", to_branch: "master", reviewers: &reviewers };
        assert_eq!(b.create_pull_request(&none(), &draft).await.unwrap()["id"], 31);
        assert!(b.create_pull_request(&none(), &draft).await.unwrap_err().to_string().contains("didn't return the new pull request"));
        let u = urls(&c);
        assert!(u[0].ends_with("role=AUTHOR&state=OPEN&order=NEWEST&limit=50") && u[1].ends_with("limit=50&participantStatus=UNAPPROVED"), "{u:?}");
        assert_eq!(u[2], "/rest/api/1.0/projects/ACME/repos/app/pull-requests/4/merge");
        assert_eq!(u[3], "/rest/api/1.0/projects/ACME/repos/app/pull-requests");
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(seen[3].body.as_deref().unwrap()).unwrap()["reviewers"], json!([{ "user": { "name": "ann" } }]));
    }

    #[tokio::test]
    async fn doctors_raw_fetchers_return_the_unnormalized_answers() {
        let c = client(vec![ok(raw_pr()), ok(json!({ "version": "9.4.16" }))]);
        assert_eq!(bb(&c).fetch_pull_request_raw(&none(), "ACME", "app", 12).await.unwrap()["properties"]["commentCount"], 3);
        assert_eq!(bb(&c).application_properties(&none()).await.unwrap().unwrap()["major"], 9);
    }

    #[tokio::test]
    async fn an_unsafe_branch_name_is_refused_before_any_request() {
        let c = client(vec![]);
        assert!(bb(&c).list_branch_pull_requests(&none(), "ACME", "app", "-rf").await.unwrap_err().to_string().contains("Not a safe branch name"));
        assert!(c.transport.seen.lock().unwrap().is_empty());
    }
}
