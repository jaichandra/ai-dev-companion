//! The seam between features and the external systems they work with (port of
//! `core/providers.ts`). A feature asks for `providers.git`, `providers.issues`
//! or `providers.ci` and never names a concrete client, so a different git host,
//! tracker or CI server is a new implementation of one trait rather than a change
//! to every feature.
//!
//! Which implementation backs each role comes from the environment profile
//! (`sites[]`: each has a `kind` and a `provider` id). The implementations wrap
//! the existing clients and bind them to the *live* config (`ConfigFn` is called
//! on every use), so a base URL or token saved from Settings applies on the next
//! call; nothing is cached across calls except the provider objects themselves.
//!
//! Optional methods are declared by `capabilities`; a feature checks the set
//! instead of assuming, so a provider without (say) default reviewers degrades
//! the feature rather than breaking it.

use crate::atlassian::{AtlassianError, AuthContext, Client};
use crate::bitbucket::{bitbucket_site, Bitbucket};
use crate::bitbucket_endpoints::{DefaultReviewersQuery, PullRequestDraft as BitbucketDraft};
use crate::github::{github_site, Github, PullRequestDraft as GithubDraft};
use crate::http::HttpTransport;
use crate::jenkins::{jenkins_site, Jenkins, JenkinsCache};
use crate::jira::{jira_site, Jira};
use crate::site_tokens::SiteTokens;
use crate::uri::encode_uri_component;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, AtlassianError>;

pub use crate::config::ConfigFn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCapability {
    DefaultReviewers,
    BuildStatus,
    Dashboard,
    MergeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueCapability {
    Transitions,
    RemoteLinks,
    LinkedPullRequests,
    Subtasks,
}

/// What opening a PR needs; `project` is the owner on GitHub.
#[derive(Debug, Clone)]
pub struct NewPullRequest {
    pub project: String,
    pub repo: String,
    pub title: String,
    pub description: String,
    pub from_branch: String,
    pub to_branch: String,
    pub reviewers: Vec<String>,
}

/// The ids Bitbucket's default-reviewer lookup needs.
#[derive(Debug, Clone)]
pub struct ReviewerIds {
    pub source_repo_id: i64,
    pub target_repo_id: i64,
    pub source_branch: String,
    pub target_branch: String,
}

#[async_trait]
pub trait GitHost: Send + Sync {
    /// The implementation's id, as named in the profile (`sites[].provider`).
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> &'static [GitCapability];
    fn has(&self, c: GitCapability) -> bool {
        self.capabilities().contains(&c)
    }
    /// The configured base URL, no trailing slash. Read live from config.
    fn base_url(&self) -> Result<String>;
    /// Whether a token is configured (saved, or borrowed from another tool).
    fn has_token(&self) -> Result<bool>;
    /// The web address of a pull request.
    fn pr_url(&self, project: &str, repo: &str, pr_id: i64) -> Result<String>;
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>>;
    async fn get_pull_request(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Option<Value>>;
    async fn list_activities(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Vec<Value>>;
    async fn reply_to_comment(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64, parent_id: i64, text: &str) -> Result<Option<i64>>;
    async fn get_default_branch(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<Option<String>>;
    async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>>;
    async fn create_pull_request(&self, auth: &AuthContext, args: &NewPullRequest) -> Result<Value>;
    async fn get_repository_id(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<i64>;
    /// With the `DefaultReviewers` capability.
    async fn get_default_reviewers(&self, auth: &AuthContext, project: &str, repo: &str, ids: &ReviewerIds) -> Result<Vec<String>>;
    /// With the `BuildStatus` capability.
    async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>>;
    /// With the `Dashboard` capability: `role` is `AUTHOR` or `REVIEWER`.
    async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>>;
    /// With the `MergeStatus` capability: `{conflicted, canMerge, outcome}`.
    async fn get_merge_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Value>;
}

#[derive(Debug, Clone)]
pub struct NewSubtask {
    pub project_key: String,
    pub parent_key: String,
    pub summary: String,
    /// A username; the provider turns it into whatever its API wants (Jira Server: `{name}`).
    pub assignee: String,
}

#[async_trait]
pub trait IssueTracker: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> &'static [IssueCapability];
    fn has(&self, c: IssueCapability) -> bool {
        self.capabilities().contains(&c)
    }
    fn base_url(&self) -> String;
    fn has_token(&self) -> bool;
    /// The web address of an issue.
    fn issue_url(&self, key: &str) -> String;
    /// Issue types a subtask can be added under.
    fn subtask_parent_types(&self) -> &[String];
    async fn search_issues(&self, auth: &AuthContext, query: &str, max_results: Option<i64>) -> Result<Vec<Value>>;
    async fn get_issue_basics(&self, auth: &AuthContext, key: &str) -> Result<Value>;
    /// The tracker's own JSON for an issue with the named fields (comma-separated).
    async fn get_issue_raw(&self, auth: &AuthContext, key: &str, fields: &str) -> Result<Value>;
    /// The tracker's own JSON for the issue's comments, oldest first.
    async fn get_issue_comments(&self, auth: &AuthContext, key: &str) -> Result<Value>;
    /// Posts a comment already in the tracker's markup; returns its id when known.
    async fn add_comment(&self, auth: &AuthContext, key: &str, body: &str) -> Result<Option<String>>;
    /// With the `Transitions` capability.
    async fn list_transitions(&self, auth: &AuthContext, key: &str) -> Result<Vec<crate::jira_workflow::Transition>>;
    async fn transition_issue(&self, auth: &AuthContext, key: &str, transition_id: &str) -> Result<()>;
    /// With the `RemoteLinks` capability.
    async fn add_remote_link(&self, auth: &AuthContext, key: &str, url: &str, title: &str) -> Result<()>;
    /// With the `RemoteLinks` capability: the tracker's own JSON for the issue's links to other systems.
    async fn get_remote_links_raw(&self, auth: &AuthContext, key: &str) -> Result<Value>;
    /// With the `LinkedPullRequests` capability: the tracker's own JSON for the PRs linked to an issue id.
    async fn get_linked_pull_requests_raw(&self, auth: &AuthContext, issue_id: &str) -> Result<Value>;
    /// With the `Subtasks` capability: the created issue (`{key}`).
    async fn create_subtask(&self, auth: &AuthContext, args: &NewSubtask) -> Result<Value>;
}

#[async_trait]
pub trait CiServer: Send + Sync {
    fn id(&self) -> &'static str;
    fn base_url(&self) -> String;
    fn has_token(&self) -> bool;
    async fn fetch_test_report(&self, auth: &AuthContext, job: &str, build_number: i64) -> Result<Option<Value>>;
}

#[derive(Clone)]
pub struct Providers {
    pub git: Arc<dyn GitHost>,
    pub issues: Arc<dyn IssueTracker>,
    pub ci: Arc<dyn CiServer>,
}

/// What every implementation is built from.
pub struct Deps<T: HttpTransport> {
    pub client: Arc<Client<T>>,
    pub tokens: Arc<SiteTokens>,
    pub config: ConfigFn,
}

impl<T: HttpTransport> Clone for Deps<T> {
    fn clone(&self) -> Self {
        Deps { client: self.client.clone(), tokens: self.tokens.clone(), config: self.config.clone() }
    }
}

// ---- GitHub and GitHub Enterprise ----

/// Maps GitHub onto the git-host trait: owner = project, repo name = repo, PR number = pr_id.
/// Token-only (a personal access token); GitHub has no default reviewers.
pub struct GithubProvider<T: HttpTransport> {
    deps: Deps<T>,
    site_id: String,
}

impl<T: HttpTransport> GithubProvider<T> {
    pub fn new(deps: Deps<T>, site_id: &str) -> Self {
        Self { deps, site_id: site_id.to_string() }
    }
    fn site(&self) -> Result<crate::github::GithubSite> {
        github_site(&(self.deps.config)(), &self.deps.tokens, &self.site_id)
    }
    fn api(&self) -> Result<Github<'_, T>> {
        Ok(Github::new(&self.deps.client, self.site()?))
    }
}

#[async_trait]
impl<T: HttpTransport> GitHost for GithubProvider<T> {
    fn id(&self) -> &'static str {
        "github"
    }
    fn capabilities(&self) -> &'static [GitCapability] {
        &[GitCapability::BuildStatus, GitCapability::Dashboard, GitCapability::MergeStatus]
    }
    fn base_url(&self) -> Result<String> {
        Ok(self.site()?.web)
    }
    fn has_token(&self) -> Result<bool> {
        Ok(self.site()?.rest.api_token.is_some())
    }
    fn pr_url(&self, owner: &str, repo: &str, pr_id: i64) -> Result<String> {
        Ok(format!("{}/{owner}/{repo}/pull/{pr_id}", self.site()?.web))
    }
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>> {
        self.api()?.who_am_i(auth).await
    }
    async fn get_pull_request(&self, auth: &AuthContext, owner: &str, repo: &str, pr_id: i64) -> Result<Option<Value>> {
        self.api()?.get_pull_request(auth, owner, repo, pr_id).await
    }
    async fn list_activities(&self, auth: &AuthContext, owner: &str, repo: &str, pr_id: i64) -> Result<Vec<Value>> {
        self.api()?.list_activities(auth, owner, repo, pr_id).await
    }
    async fn reply_to_comment(&self, auth: &AuthContext, owner: &str, repo: &str, pr_id: i64, parent_id: i64, text: &str) -> Result<Option<i64>> {
        self.api()?.reply_to_comment(auth, owner, repo, pr_id, parent_id, text).await
    }
    async fn get_default_branch(&self, auth: &AuthContext, owner: &str, repo: &str) -> Result<Option<String>> {
        self.api()?.get_default_branch(auth, owner, repo).await
    }
    async fn list_branch_pull_requests(&self, auth: &AuthContext, owner: &str, repo: &str, branch: &str) -> Result<Vec<Value>> {
        self.api()?.list_branch_pull_requests(auth, owner, repo, branch).await
    }
    async fn create_pull_request(&self, auth: &AuthContext, a: &NewPullRequest) -> Result<Value> {
        let draft = GithubDraft { owner: &a.project, repo: &a.repo, title: &a.title, description: Some(&a.description), from_branch: &a.from_branch, to_branch: &a.to_branch, reviewers: &a.reviewers };
        self.api()?.create_pull_request(auth, &draft).await
    }
    async fn get_repository_id(&self, auth: &AuthContext, owner: &str, repo: &str) -> Result<i64> {
        self.api()?.get_repository_id(auth, owner, repo).await
    }
    async fn get_default_reviewers(&self, _: &AuthContext, _: &str, _: &str, _: &ReviewerIds) -> Result<Vec<String>> {
        self.api()?.get_default_reviewers().await
    }
    async fn get_commit_build_status(&self, auth: &AuthContext, owner: &str, repo: &str, sha: &str) -> Result<Option<Value>> {
        self.api()?.get_commit_build_status(auth, owner, repo, sha).await
    }
    async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>> {
        self.api()?.list_dashboard_pull_requests(auth, role).await
    }
    async fn get_merge_status(&self, auth: &AuthContext, owner: &str, repo: &str, pr_id: i64) -> Result<Value> {
        self.api()?.get_merge_status(auth, owner, repo, pr_id).await
    }
}

// ---- Bitbucket Server / Data Center ----

pub struct BitbucketDcProvider<T: HttpTransport> {
    deps: Deps<T>,
}

impl<T: HttpTransport> BitbucketDcProvider<T> {
    pub fn new(deps: Deps<T>) -> Self {
        Self { deps }
    }
    fn api(&self) -> Bitbucket<'_, T> {
        Bitbucket { client: &self.deps.client, site: bitbucket_site(&(self.deps.config)(), &self.deps.tokens) }
    }
}

#[async_trait]
impl<T: HttpTransport> GitHost for BitbucketDcProvider<T> {
    fn id(&self) -> &'static str {
        "bitbucket-dc"
    }
    fn capabilities(&self) -> &'static [GitCapability] {
        &[GitCapability::DefaultReviewers, GitCapability::BuildStatus, GitCapability::Dashboard, GitCapability::MergeStatus]
    }
    fn base_url(&self) -> Result<String> {
        Ok(self.api().site.base_url)
    }
    fn has_token(&self) -> Result<bool> {
        Ok(self.api().site.api_token.is_some())
    }
    fn pr_url(&self, project: &str, repo: &str, pr_id: i64) -> Result<String> {
        Ok(format!("{}/projects/{project}/repos/{repo}/pull-requests/{pr_id}", self.api().site.base_url))
    }
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>> {
        self.api().who_am_i(auth).await
    }
    async fn get_pull_request(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Option<Value>> {
        self.api().get_pull_request(auth, project, repo, pr_id).await
    }
    async fn list_activities(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Vec<Value>> {
        self.api().list_activities(auth, project, repo, pr_id).await
    }
    async fn reply_to_comment(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64, parent_id: i64, text: &str) -> Result<Option<i64>> {
        self.api().reply_to_comment(auth, project, repo, pr_id, parent_id, text).await
    }
    async fn get_default_branch(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<Option<String>> {
        self.api().get_default_branch(auth, project, repo).await
    }
    async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>> {
        self.api().list_branch_pull_requests(auth, project, repo, branch).await
    }
    async fn create_pull_request(&self, auth: &AuthContext, a: &NewPullRequest) -> Result<Value> {
        let (title, description) = (json!(a.title), json!(a.description));
        let draft = BitbucketDraft { project: &a.project, repo: &a.repo, title: &title, description: &description, from_branch: &a.from_branch, to_branch: &a.to_branch, reviewers: &a.reviewers };
        self.api().create_pull_request(auth, &draft).await
    }
    async fn get_repository_id(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<i64> {
        self.api().get_repository_id(auth, project, repo).await
    }
    async fn get_default_reviewers(&self, auth: &AuthContext, project: &str, repo: &str, ids: &ReviewerIds) -> Result<Vec<String>> {
        let q = DefaultReviewersQuery { source_repo_id: ids.source_repo_id, target_repo_id: ids.target_repo_id, source_branch: &ids.source_branch, target_branch: &ids.target_branch };
        self.api().get_default_reviewers(auth, project, repo, &q).await
    }
    async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>> {
        self.api().get_commit_build_status(auth, project, repo, sha).await
    }
    async fn list_dashboard_pull_requests(&self, auth: &AuthContext, role: &str) -> Result<Vec<Value>> {
        self.api().list_dashboard_pull_requests(auth, role).await
    }
    async fn get_merge_status(&self, auth: &AuthContext, project: &str, repo: &str, pr_id: i64) -> Result<Value> {
        self.api().get_merge_status(auth, project, repo, pr_id).await
    }
}

// ---- Jira Server / Data Center ----

pub struct JiraDcProvider<T: HttpTransport> {
    deps: Deps<T>,
    subtask_type_name: String,
    subtask_parent_types: Vec<String>,
}

impl<T: HttpTransport> JiraDcProvider<T> {
    /// `issues` is the profile's `issues` section (`subtaskTypeName`, `subtaskParentTypes`).
    pub fn new(deps: Deps<T>, issues: &Value) -> Self {
        let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Self { deps, subtask_type_name: issues["subtaskTypeName"].as_str().unwrap_or("").to_string(), subtask_parent_types: strings(&issues["subtaskParentTypes"]) }
    }
    fn site(&self) -> crate::atlassian::SiteAuth {
        jira_site(&(self.deps.config)(), &self.deps.tokens)
    }
    fn api(&self) -> Jira<'_, T> {
        Jira { client: &self.deps.client, site: self.site() }
    }
}

fn issue_path(key: &str) -> String {
    format!("/rest/api/2/issue/{}", encode_uri_component(key))
}

#[async_trait]
impl<T: HttpTransport> IssueTracker for JiraDcProvider<T> {
    fn id(&self) -> &'static str {
        "jira-dc"
    }
    fn capabilities(&self) -> &'static [IssueCapability] {
        &[IssueCapability::Transitions, IssueCapability::RemoteLinks, IssueCapability::LinkedPullRequests, IssueCapability::Subtasks]
    }
    fn base_url(&self) -> String {
        self.site().base_url
    }
    fn has_token(&self) -> bool {
        self.site().api_token.is_some()
    }
    fn issue_url(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.site().base_url.trim_end_matches('/'))
    }
    fn subtask_parent_types(&self) -> &[String] {
        &self.subtask_parent_types
    }
    async fn search_issues(&self, auth: &AuthContext, query: &str, max_results: Option<i64>) -> Result<Vec<Value>> {
        self.api().search_issues(auth, query, max_results.unwrap_or(20)).await
    }
    async fn get_issue_basics(&self, auth: &AuthContext, key: &str) -> Result<Value> {
        self.api().get_issue_basics(auth, key).await
    }
    async fn get_issue_raw(&self, auth: &AuthContext, key: &str, fields: &str) -> Result<Value> {
        self.deps.client.get(&self.site(), auth, &format!("{}?fields={fields}", issue_path(key))).await
    }
    async fn get_issue_comments(&self, auth: &AuthContext, key: &str) -> Result<Value> {
        self.api().get_issue_comments(auth, key).await
    }
    async fn add_comment(&self, auth: &AuthContext, key: &str, body: &str) -> Result<Option<String>> {
        let created = self.deps.client.json(&self.site(), auth, &format!("{}/comment", issue_path(key)), Some("POST"), Some(&json!({ "body": body }))).await?;
        Ok(match &created["id"] {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    }
    async fn list_transitions(&self, auth: &AuthContext, key: &str) -> Result<Vec<crate::jira_workflow::Transition>> {
        self.api().list_transitions(auth, key).await
    }
    async fn transition_issue(&self, auth: &AuthContext, key: &str, transition_id: &str) -> Result<()> {
        self.api().transition_issue(auth, key, transition_id).await
    }
    async fn add_remote_link(&self, auth: &AuthContext, key: &str, url: &str, title: &str) -> Result<()> {
        self.api().add_remote_link(auth, key, url, title).await
    }
    async fn get_remote_links_raw(&self, auth: &AuthContext, key: &str) -> Result<Value> {
        self.deps.client.get(&self.site(), auth, &format!("{}/remotelink", issue_path(key))).await
    }
    // applicationType=stash is the Bitbucket Server link in Jira's development panel.
    async fn get_linked_pull_requests_raw(&self, auth: &AuthContext, issue_id: &str) -> Result<Value> {
        self.deps.client.get(&self.site(), auth, &format!("/rest/dev-status/latest/issue/detail?issueId={issue_id}&applicationType=stash&dataType=pullrequest")).await
    }
    async fn create_subtask(&self, auth: &AuthContext, a: &NewSubtask) -> Result<Value> {
        let body = json!({ "fields": {
            "project": { "key": a.project_key },
            "parent": { "key": a.parent_key },
            "summary": a.summary,
            "issuetype": { "name": self.subtask_type_name },
            "assignee": { "name": a.assignee },
        } });
        self.deps.client.json(&self.site(), auth, "/rest/api/2/issue", Some("POST"), Some(&body)).await
    }
}

// ---- Jenkins ----

pub struct JenkinsProvider<T: HttpTransport> {
    deps: Deps<T>,
    cache: JenkinsCache,
}

impl<T: HttpTransport> JenkinsProvider<T> {
    pub fn new(deps: Deps<T>) -> Self {
        Self { deps, cache: JenkinsCache::default() }
    }
    fn site(&self) -> crate::atlassian::SiteAuth {
        jenkins_site(&(self.deps.config)(), &self.deps.tokens)
    }
}

#[async_trait]
impl<T: HttpTransport> CiServer for JenkinsProvider<T> {
    fn id(&self) -> &'static str {
        "jenkins"
    }
    fn base_url(&self) -> String {
        self.site().base_url
    }
    fn has_token(&self) -> bool {
        self.site().api_token.is_some()
    }
    async fn fetch_test_report(&self, auth: &AuthContext, job: &str, build_number: i64) -> Result<Option<Value>> {
        Jenkins::new(&self.deps.client, self.site(), &self.cache).fetch_test_report(auth, job, build_number, true).await
    }
}

// ---- Choosing the implementations ----

/// The git host named by a profile site's `provider`, or an error listing the known ones.
pub fn git_provider<T: HttpTransport + 'static>(deps: Deps<T>, provider: &str, site_id: &str) -> std::result::Result<Arc<dyn GitHost>, String> {
    match provider {
        "bitbucket-dc" => Ok(Arc::new(BitbucketDcProvider::new(deps))),
        "github" => Ok(Arc::new(GithubProvider::new(deps, site_id))),
        other => Err(format!("No git provider named \"{other}\" (known: bitbucket-dc, github).")),
    }
}

pub fn issues_provider<T: HttpTransport + 'static>(deps: Deps<T>, provider: &str, issues: &Value) -> std::result::Result<Arc<dyn IssueTracker>, String> {
    match provider {
        "jira-dc" => Ok(Arc::new(JiraDcProvider::new(deps, issues))),
        other => Err(format!("No issues provider named \"{other}\" (known: jira-dc).")),
    }
}

pub fn ci_provider<T: HttpTransport + 'static>(deps: Deps<T>, provider: &str) -> std::result::Result<Arc<dyn CiServer>, String> {
    match provider {
        "jenkins" => Ok(Arc::new(JenkinsProvider::new(deps))),
        other => Err(format!("No ci provider named \"{other}\" (known: jenkins).")),
    }
}

/// The providers the environment profile names, one per role.
pub fn create_providers<T: HttpTransport + 'static>(deps: Deps<T>) -> std::result::Result<Providers, String> {
    let env = crate::environment::current();
    let site = |kind: &str| env.sites.iter().find(|s| s.kind == kind).ok_or_else(|| format!("The environment profile has no \"{kind}\" site."));
    let git = site("git")?;
    let issues = site("issues")?;
    let ci = site("ci")?;
    let issues_section = env.profile.get("issues").cloned().unwrap_or(Value::Null);
    Ok(Providers {
        git: git_provider(deps.clone(), &git.provider, &git.id)?,
        issues: issues_provider(deps.clone(), &issues.provider, &issues_section)?,
        ci: ci_provider(deps, &ci.provider)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_context::AuthHub;
    use crate::http::testing::FakeTransport;
    use crate::http::HttpResponse;
    use crate::site_tokens::testing::{tokens, Fixed};
    use std::sync::Mutex;

    fn deps(config: Arc<Mutex<Value>>, saved: Vec<(&'static str, &'static str)>, reply: Value) -> Deps<FakeTransport> {
        let t = FakeTransport::new(move |_| Ok(HttpResponse { status: 200, headers: vec![], body: reply.to_string().into_bytes() }));
        let _ = Fixed(vec![]);
        Deps { client: Arc::new(Client::new(t, Arc::new(AuthHub::new(Arc::new(Fixed(saved.clone())))))), tokens: Arc::new(tokens(saved)), config: Arc::new(move || config.lock().unwrap().clone()) }
    }
    fn none() -> AuthContext {
        AuthContext::default()
    }

    #[tokio::test]
    async fn the_github_provider_reads_the_live_config_each_call() {
        let config = Arc::new(Mutex::new(json!({})));
        let d = deps(config.clone(), vec![("github.apiToken", "t")], json!({ "login": "JDoe" }));
        let p = git_provider(d.clone(), "github", "github").unwrap();
        assert_eq!((p.id(), p.base_url().unwrap().as_str(), p.has_token().unwrap()), ("github", "https://github.com", true));
        assert_eq!(p.pr_url("o", "r", 5).unwrap(), "https://github.com/o/r/pull/5");
        assert!(p.has(GitCapability::MergeStatus) && !p.has(GitCapability::DefaultReviewers));
        *config.lock().unwrap() = json!({ "github": { "baseUrl": "https://ghe.example.com/" } });
        assert_eq!(p.base_url().unwrap(), "https://ghe.example.com", "a Settings change applies without rebuilding");
        assert_eq!(p.who_am_i(&none()).await.unwrap().as_deref(), Some("jdoe"));
        assert!(d.client.transport.seen.lock().unwrap()[0].url.starts_with("https://ghe.example.com/api/v3/user"));
        assert!(p.get_default_reviewers(&none(), "o", "r", &ReviewerIds { source_repo_id: 1, target_repo_id: 1, source_branch: "a".into(), target_branch: "b".into() }).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_bitbucket_provider_builds_urls_and_asks_for_default_reviewers_by_ids() {
        let config = Arc::new(Mutex::new(json!({ "bitbucket": { "baseUrl": "https://bb.example.com" } })));
        let d = deps(config.clone(), vec![], json!({ "values": [] }));
        let p = git_provider(d.clone(), "bitbucket-dc", "bitbucket").unwrap();
        assert_eq!(p.pr_url("ACME", "app", 7).unwrap(), "https://bb.example.com/projects/ACME/repos/app/pull-requests/7");
        assert!(!p.has_token().unwrap());
        assert!(p.has(GitCapability::DefaultReviewers));
        let d = deps(config, vec![("bitbucket.apiToken", "t")], json!({ "values": [] }));
        let p = git_provider(d.clone(), "bitbucket-dc", "bitbucket").unwrap();
        let ids = ReviewerIds { source_repo_id: 1, target_repo_id: 2, source_branch: "f".into(), target_branch: "master".into() };
        assert!(p.get_default_reviewers(&none(), "ACME", "app", &ids).await.unwrap().is_empty());
        assert!(d.client.transport.seen.lock().unwrap()[0].url.contains("/default-reviewers"));
    }

    #[tokio::test]
    async fn the_jira_provider_wraps_raw_reads_and_stringifies_a_new_comment_id() {
        let config = Arc::new(Mutex::new(json!({ "jira": { "baseUrl": "https://jira.example.com/" } })));
        let d = deps(config, vec![("jira.apiToken", "t")], json!({ "id": 10001, "key": "A-2" }));
        let issues = json!({ "subtaskTypeName": "Sub-task", "subtaskParentTypes": ["Story", "Task"] });
        let p = issues_provider(d.clone(), "jira-dc", &issues).unwrap();
        assert_eq!((p.id(), p.issue_url("A-1").as_str(), p.has_token()), ("jira-dc", "https://jira.example.com/browse/A-1", true));
        assert_eq!(p.subtask_parent_types(), ["Story", "Task"]);
        assert_eq!(p.add_comment(&none(), "A-1", "hi").await.unwrap().as_deref(), Some("10001"));
        p.get_issue_raw(&none(), "A-1", "summary,status").await.unwrap();
        p.get_linked_pull_requests_raw(&none(), "42").await.unwrap();
        let created = p.create_subtask(&none(), &NewSubtask { project_key: "A".into(), parent_key: "A-1".into(), summary: "S".into(), assignee: "jdoe".into() }).await.unwrap();
        assert_eq!(created["key"], "A-2");
        let seen = d.client.transport.seen.lock().unwrap();
        assert!(seen[1].url.ends_with("/rest/api/2/issue/A-1?fields=summary,status"));
        assert!(seen[2].url.ends_with("/rest/dev-status/latest/issue/detail?issueId=42&applicationType=stash&dataType=pullrequest"));
        let body: Value = serde_json::from_slice(seen[3].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["fields"], json!({ "project": { "key": "A" }, "parent": { "key": "A-1" }, "summary": "S", "issuetype": { "name": "Sub-task" }, "assignee": { "name": "jdoe" } }));
    }

    #[tokio::test]
    async fn the_ci_provider_keeps_its_report_cache_across_calls() {
        let config = Arc::new(Mutex::new(json!({ "jenkins": { "username": "jdoe" } })));
        let d = deps(config, vec![("jenkins.apiToken", "t")], json!({ "failCount": 0, "passCount": 1, "skipCount": 0, "suites": [] }));
        let p = ci_provider(d.clone(), "jenkins").unwrap();
        assert!(p.has_token());
        p.fetch_test_report(&none(), "job", 3).await.unwrap().unwrap();
        p.fetch_test_report(&none(), "job", 3).await.unwrap().unwrap();
        assert_eq!(d.client.transport.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unknown_provider_names_the_known_ones_and_the_profile_picks_one_per_role() {
        let d = deps(Arc::new(Mutex::new(json!({}))), vec![], json!({}));
        assert_eq!(git_provider(d.clone(), "gitlab", "x").err().unwrap(), "No git provider named \"gitlab\" (known: bitbucket-dc, github).");
        assert!(issues_provider(d.clone(), "linear", &json!({})).err().unwrap().contains("known: jira-dc"));
        assert!(ci_provider(d.clone(), "teamcity").is_err());
        let p = create_providers(d).unwrap();
        assert_eq!((p.git.id(), p.issues.id(), p.ci.id()), ("github", "jira-dc", "jenkins"), "the placeholder profile");
    }
}
