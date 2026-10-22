//! Jira as an authenticated site plus the calls Ticket to PR and the readers
//! make (port of `core/jira.ts`). Bearer token (`SiteAuth`'s default scheme),
//! the same convention as `bitbucket_site`.

use crate::atlassian::{AtlassianError, AuthContext, Client, SiteAuth};
use crate::http::HttpTransport;
use crate::jira_workflow as workflow;
use crate::site_tokens::SiteTokens;
use crate::uri::encode_uri_component;
use serde_json::{json, Value};

pub type Result<T> = std::result::Result<T, AtlassianError>;

const SEARCH_FIELDS: &str = "summary,status,issuetype,priority,updated,description";

/// Jira as an authenticated site. The base URL is used as configured (no normalizing), as in Node.
pub fn jira_site(config: &Value, tokens: &SiteTokens) -> SiteAuth {
    let base_url = config["jira"]["baseUrl"].as_str().filter(|b| !b.is_empty()).map(str::to_string).unwrap_or_else(|| crate::environment::current().default_base_url("jira").to_string());
    // Own saved token first, then the one another tool already stored for this host.
    let api_token = tokens.token("jira.apiToken", &base_url);
    SiteAuth { base_url, label: "Jira".into(), config_key: "jira.apiToken".into(), api_token, ..Default::default() }
}

// Issue keys reach here already validated (features' parse_issue_key); the encoding is belt and braces.
fn issue_path(key: &str) -> String {
    format!("/rest/api/2/issue/{}", encode_uri_component(key))
}

pub struct Jira<'a, T: HttpTransport> {
    pub client: &'a Client<T>,
    pub site: SiteAuth,
}

impl<T: HttpTransport> Jira<'_, T> {
    /// Up to `max_results` (at most 50, at least 1) issues matching `jql`.
    pub async fn search_issues(&self, auth: &AuthContext, jql: &str, max_results: i64) -> Result<Vec<Value>> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("jql", jql)
            .append_pair("fields", SEARCH_FIELDS)
            .append_pair("maxResults", &max_results.clamp(1, 50).to_string())
            .finish();
        let raw = self.client.get(&self.site, auth, &format!("/rest/api/2/search?{query}")).await?;
        Ok(workflow::normalize_search_results(&raw, Some(&self.site.base_url)))
    }

    pub async fn get_issue_basics(&self, auth: &AuthContext, key: &str) -> Result<Value> {
        let raw = self.client.get(&self.site, auth, &format!("{}?fields=summary,issuetype,status", issue_path(key))).await?;
        workflow::normalize_issue_basics(&raw).ok_or_else(|| AtlassianError::Other(format!("Jira didn't return {key}.")))
    }

    /// Adds (or, by globalId, updates) the ticket's link to a PR.
    pub async fn add_remote_link(&self, auth: &AuthContext, key: &str, url: &str, title: &str) -> Result<()> {
        let body = workflow::remote_link_body(Some(url), Some(title))?;
        self.client.json(&self.site, auth, &format!("{}/remotelink", issue_path(key)), Some("POST"), Some(&body)).await?;
        Ok(())
    }

    pub async fn list_transitions(&self, auth: &AuthContext, key: &str) -> Result<Vec<workflow::Transition>> {
        Ok(workflow::normalize_transitions(&self.client.get(&self.site, auth, &format!("{}/transitions", issue_path(key))).await?))
    }

    pub async fn transition_issue(&self, auth: &AuthContext, key: &str, transition_id: &str) -> Result<()> {
        let body = json!({ "transition": { "id": transition_id } });
        self.client.json(&self.site, auth, &format!("{}/transitions", issue_path(key)), Some("POST"), Some(&body)).await?;
        Ok(())
    }

    /// The raw comment list of an issue (up to 1000, oldest first); the summarize-comments plan normalizes it.
    pub async fn get_issue_comments(&self, auth: &AuthContext, key: &str) -> Result<Value> {
        self.client.get(&self.site, auth, &format!("{}/comment?maxResults=1000&orderBy=created", issue_path(key))).await
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
    fn ok(body: Value) -> Answer {
        Ok(HttpResponse { status: 200, headers: vec![], body: body.to_string().into_bytes() })
    }
    fn client(answers: Vec<Answer>) -> Client<FakeTransport> {
        let q = Mutex::new(VecDeque::from(answers));
        let t = FakeTransport::new(move |_| q.lock().unwrap().pop_front().expect("more calls than answers"));
        Client::new(t, Arc::new(AuthHub::new(Arc::new(Fixed(vec![])))))
    }
    fn jira(c: &Client<FakeTransport>) -> Jira<'_, FakeTransport> {
        Jira { client: c, site: SiteAuth { base_url: "https://jira.example.com".into(), label: "Jira".into(), config_key: "jira.apiToken".into(), api_token: Some("t".into()), ..Default::default() } }
    }
    fn url_of(c: &Client<FakeTransport>, i: usize) -> String {
        c.transport.seen.lock().unwrap()[i].url.replace("https://jira.example.com", "")
    }
    fn none() -> AuthContext {
        AuthContext::default()
    }

    #[test]
    fn the_site_takes_the_configured_base_url_as_is_and_the_saved_token() {
        let s = jira_site(&json!({ "jira": { "baseUrl": "https://j.example.com/" } }), &tokens(vec![("jira.apiToken", "own")]));
        assert_eq!((s.base_url.as_str(), s.label.as_str(), s.config_key.as_str(), s.api_token.as_deref()), ("https://j.example.com/", "Jira", "jira.apiToken", Some("own")));
        assert_eq!(jira_site(&json!({}), &tokens(vec![])).api_token, None);
    }

    #[tokio::test]
    async fn search_form_encodes_the_query_and_clamps_the_page_size() {
        let c = client(vec![ok(json!({ "issues": [{ "key": "A-1", "fields": { "summary": "S", "status": { "name": "Open", "statusCategory": { "name": "To Do" } } } }] })), ok(json!({ "issues": [] })), ok(json!({ "issues": [] }))]);
        let j = jira(&c);
        let hits = j.search_issues(&none(), "project = A AND text ~ \"x y\"", 20).await.unwrap();
        assert_eq!((hits[0]["key"].clone(), hits[0]["url"].clone()), (json!("A-1"), json!("https://jira.example.com/browse/A-1")));
        j.search_issues(&none(), "x", 500).await.unwrap();
        j.search_issues(&none(), "x", 0).await.unwrap();
        assert_eq!(url_of(&c, 0), "/rest/api/2/search?jql=project+%3D+A+AND+text+%7E+%22x+y%22&fields=summary%2Cstatus%2Cissuetype%2Cpriority%2Cupdated%2Cdescription&maxResults=20");
        assert!(url_of(&c, 1).ends_with("maxResults=50") && url_of(&c, 2).ends_with("maxResults=1"));
    }

    #[tokio::test]
    async fn issue_basics_transitions_links_and_comments_use_the_issue_paths() {
        let c = client(vec![
            ok(json!({ "key": "A-1", "fields": { "summary": "S", "issuetype": { "name": "Bug" }, "status": { "name": "Open" } } })),
            ok(json!({})),
            ok(json!({ "transitions": [{ "id": "31", "name": "Start", "to": { "name": "In Progress" } }] })),
            ok(json!({})),
            ok(json!({})),
            ok(json!({ "comments": [] })),
            ok(json!({ "unexpected": true })),
        ]);
        let j = jira(&c);
        let b = j.get_issue_basics(&none(), "A-1").await.unwrap();
        assert_eq!((b["key"].clone(), b["issueType"].clone(), b["status"].clone()), (json!("A-1"), json!("Bug"), json!("Open")));
        j.add_remote_link(&none(), "A-1", "https://bb.example.com/pr/1", "PR 1").await.unwrap();
        let t = j.list_transitions(&none(), "A-1").await.unwrap();
        assert_eq!((t[0].id.as_str(), t[0].to.as_deref()), ("31", Some("In Progress")));
        j.transition_issue(&none(), "A-1", "31").await.unwrap();
        assert!(j.add_remote_link(&none(), "A-1", "", "x").await.is_err(), "a link without a url is refused before any request");
        j.get_issue_comments(&none(), "A-1").await.unwrap();
        assert_eq!(url_of(&c, 0), "/rest/api/2/issue/A-1?fields=summary,issuetype,status");
        assert_eq!(url_of(&c, 1), "/rest/api/2/issue/A-1/remotelink");
        assert_eq!(url_of(&c, 3), "/rest/api/2/issue/A-1/transitions");
        assert_eq!(url_of(&c, 4), "/rest/api/2/issue/A-1/comment?maxResults=1000&orderBy=created");
        let seen = c.transport.seen.lock().unwrap();
        assert_eq!((seen[3].method.as_str(), serde_json::from_slice::<Value>(seen[3].body.as_deref().unwrap()).unwrap()), ("POST", json!({ "transition": { "id": "31" } })));
        assert_eq!(seen[1].method, "POST");
        assert_eq!(seen.len(), 5);
    }

    #[tokio::test]
    async fn a_reply_without_issue_basics_is_an_error_naming_the_key() {
        let c = client(vec![ok(json!("nope"))]);
        assert_eq!(jira(&c).get_issue_basics(&none(), "A-9").await.unwrap_err().to_string(), "Jira didn't return A-9.");
    }
}
