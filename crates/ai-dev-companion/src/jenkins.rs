//! "How does a feature talk to Jenkins" (port of `core/jenkins.ts`), mirroring
//! `bitbucket.rs`. Auth reuses `atlassian` wholesale: the relayed browser
//! session first, a configured token as fallback, `AuthSetup` otherwise. The
//! Jenkins-specific part is the scheme: HTTP Basic with username + API token.
//!
//! Everything else here keeps response sizes sane. Jenkins' REST API takes a
//! `tree=` parameter selecting exactly the fields wanted across all builds of a
//! job in one request, which turns a per-build walk into a few calls and, for
//! test reports, is the difference between 0.8-1.8 MB of stack traces per build
//! and a few tens of KB. Every endpoint is a tree projection, never a bare api/json.

use crate::atlassian::{AtlassianError, AuthContext, AuthScheme, Client, SiteAuth};
use crate::http::HttpTransport;
use crate::site_tokens::{normalize_base_url, SiteTokens};
use crate::uri::encode_uri_component;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, AtlassianError>;

const RETRYABLE_ATTEMPTS: u32 = 3;

/// Jenkins as an authenticated site. `username` is required only for the token
/// fallback: a developer already logged into Blue Ocean in this browser needs
/// neither, since the extension relays that session.
pub fn jenkins_site(config: &Value, tokens: &SiteTokens) -> SiteAuth {
    let default = crate::environment::current().default_base_url("jenkins").to_string();
    let base_url = normalize_base_url(config["jenkins"]["baseUrl"].as_str().filter(|b| !b.is_empty()).unwrap_or(&default));
    // Own saved token first (with the configured username); otherwise the
    // username and password another tool stored for this same host.
    let found = tokens.resolve("jenkins.apiToken", &base_url);
    let configured = config["jenkins"]["username"].as_str().filter(|u| !u.is_empty()).map(str::to_string);
    SiteAuth {
        username: configured.or_else(|| found.as_ref().and_then(|t| t.username.clone()).filter(|u| !u.is_empty())),
        api_token: found.map(|t| t.token),
        base_url,
        label: "Jenkins".into(),
        config_key: "jenkins.apiToken".into(),
        auth_scheme: AuthScheme::Basic,
        ..Default::default()
    }
}

/// Jenkins here intermittently drops connections mid-response, and a transient
/// 5xx is just as likely as a real one. Neither is worth failing a whole scan
/// over, so a request gets a couple of quick retries; an auth problem or a 404
/// is a settled answer and is never retried.
fn is_retryable(err: &AtlassianError) -> bool {
    match err {
        AtlassianError::AuthSetup(_) | AtlassianError::Other(_) => false,
        AtlassianError::Status { status, .. } => *status >= 500,
        AtlassianError::Transport(_) => true,
    }
}

/// A finished build's result never changes, so re-opening the panel should not
/// re-download anything already seen. Only real reports are held (a 404 is not
/// cached, see `fetch_test_report`); a still-building build is never cached.
/// In-memory: a service restart just re-fetches.
#[derive(Default)]
pub struct JenkinsCache {
    reports: Mutex<HashMap<String, Value>>,
}

impl JenkinsCache {
    pub fn clear(&self) {
        self.reports.lock().unwrap().clear();
    }
}

pub struct Jenkins<'a, T: HttpTransport> {
    pub client: &'a Client<T>,
    pub site: SiteAuth,
    pub cache: &'a JenkinsCache,
    /// The wait before retry n is `retry_ms * n`; 250 in production.
    pub retry_ms: u64,
}

impl<'a, T: HttpTransport> Jenkins<'a, T> {
    pub fn new(client: &'a Client<T>, site: SiteAuth, cache: &'a JenkinsCache) -> Self {
        Self { client, site, cache, retry_ms: 250 }
    }

    async fn json(&self, auth: &AuthContext, path_and_query: &str) -> Result<Value> {
        let mut attempt = 1;
        loop {
            match self.client.get(&self.site, auth, path_and_query).await {
                Ok(v) => return Ok(v),
                Err(e) if !is_retryable(&e) || attempt == RETRYABLE_ATTEMPTS => return Err(e),
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(self.retry_ms * u64::from(attempt))).await;
                    attempt += 1;
                }
            }
        }
    }

    /// `GET /job/<job>/api/json?tree=builds[<fields>]{0,limit}`: one request for
    /// `limit` builds' worth of whatever fields the caller names.
    pub async fn job_builds(&self, auth: &AuthContext, job: &str, fields: &str, limit: u32) -> Result<Vec<Value>> {
        let tree = encode_uri_component(&format!("builds[{fields}]{{0,{limit}}}"));
        let data = self.json(auth, &format!("/job/{}/api/json?tree={tree}", encode_uri_component(job))).await?;
        Ok(data["builds"].as_array().cloned().unwrap_or_default())
    }

    /// A build's junit report, or `None` when it published none.
    ///
    /// The 404 is normal, not exceptional: real browser builds have no test
    /// report at all, and a scan must record that as "unknown" rather than
    /// abandoning the whole table. The tree projection keeps the report small
    /// (no stack traces or stdout); `errorDetails`, a single line, is kept.
    pub async fn fetch_test_report(&self, auth: &AuthContext, job: &str, build_number: i64, cacheable: bool) -> Result<Option<Value>> {
        let key = format!("{job}#{build_number}");
        if cacheable {
            if let Some(hit) = self.cache.reports.lock().unwrap().get(&key) {
                return Ok(Some(hit.clone()));
            }
        }
        let tree = encode_uri_component("failCount,passCount,skipCount,suites[name,cases[className,name,status,age,failedSince,errorDetails,skipped]]");
        let path = format!("/job/{}/{build_number}/testReport/api/json?tree={tree}", encode_uri_component(job));
        let report = match self.json(auth, &path).await {
            Ok(r) => r,
            // Deliberately NOT cached. A 404 normally means "this build published no
            // test report", but it is indistinguishable from a build Jenkins has since
            // rotated away or a 404 during a reload; remembering one for the life of
            // the process would quietly shorten every later walk-back.
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        if cacheable {
            self.cache.reports.lock().unwrap().insert(key, report.clone());
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_context::AuthHub;
    use crate::http::testing::FakeTransport;
    use crate::http::{HttpResponse, TransportError};
    use crate::site_tokens::testing::{tokens, Fixed};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Arc;

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
    fn site() -> SiteAuth {
        SiteAuth { base_url: "https://jenkins.example.com".into(), label: "Jenkins".into(), config_key: "jenkins.apiToken".into(), username: Some("jdoe".into()), api_token: Some("tok".into()), auth_scheme: AuthScheme::Basic, ..Default::default() }
    }
    fn none() -> AuthContext {
        AuthContext::default()
    }
    fn count(c: &Client<FakeTransport>) -> usize {
        c.transport.seen.lock().unwrap().len()
    }
    fn jk<'a>(c: &'a Client<FakeTransport>, cache: &'a JenkinsCache) -> Jenkins<'a, FakeTransport> {
        let mut j = Jenkins::new(c, site(), cache);
        j.retry_ms = 0;
        j
    }

    fn bob() -> crate::packs::TokenSource {
        Arc::new(|_| json!({ "jenkins.apiToken": { "token": "pw", "origin": "https://jenkins.example.com", "source": "x", "username": "bob" } }))
    }

    #[test]
    fn the_site_is_basic_auth_with_the_configured_username_else_the_borrowed_one() {
        let s = jenkins_site(&json!({ "jenkins": { "baseUrl": "jenkins.example.com/", "username": "me" } }), &tokens(vec![("jenkins.apiToken", "own")]));
        assert_eq!((s.base_url.as_str(), s.username.as_deref(), s.api_token.as_deref(), s.auth_scheme), ("https://jenkins.example.com", Some("me"), Some("own"), AuthScheme::Basic));
        assert_eq!(jenkins_site(&json!({}), &tokens(vec![])).username, None);
        let borrowed = bob();
        let t = SiteTokens::new(Arc::new(Fixed(vec![])), crate::external_tokens::ExternalTokens::new("/h".into(), vec![borrowed]));
        let s = jenkins_site(&json!({ "jenkins": { "baseUrl": "https://jenkins.example.com" } }), &t);
        assert_eq!((s.username.as_deref(), s.api_token.as_deref()), (Some("bob"), Some("pw")));
        let saved_wins = SiteTokens::new(Arc::new(Fixed(vec![("jenkins.apiToken", "own")])), crate::external_tokens::ExternalTokens::new("/h".into(), vec![bob()]));
        let s = jenkins_site(&json!({ "jenkins": { "baseUrl": "https://jenkins.example.com" } }), &saved_wins);
        assert_eq!((s.username, s.api_token.as_deref()), (None, Some("own")), "a saved token never takes the borrowed username");
    }

    #[tokio::test]
    async fn builds_are_one_tree_projected_request_and_a_missing_list_is_empty() {
        let c = client(vec![ok(json!({ "builds": [{ "number": 3, "result": "SUCCESS", "building": false }] })), ok(json!({}))]);
        let cache = JenkinsCache::default();
        let j = jk(&c, &cache);
        assert_eq!(j.job_builds(&none(), "my job", "number,result", 5).await.unwrap().len(), 1);
        assert!(j.job_builds(&none(), "my job", "number", 5).await.unwrap().is_empty());
        assert_eq!(c.transport.seen.lock().unwrap()[0].url, "https://jenkins.example.com/job/my%20job/api/json?tree=builds%5Bnumber%2Cresult%5D%7B0%2C5%7D");
    }

    #[tokio::test]
    async fn a_5xx_and_a_dropped_connection_are_retried_but_a_4xx_is_not() {
        let c = client(vec![status(503, json!({})), Err(TransportError::ConnectionReset), ok(json!({ "builds": [] }))]);
        let cache = JenkinsCache::default();
        assert!(jk(&c, &cache).job_builds(&none(), "j", "number", 1).await.unwrap().is_empty());
        assert_eq!(count(&c), 3);
        let c = client(vec![status(500, json!({})), status(500, json!({})), status(500, json!({})), ok(json!({}))]);
        assert_eq!(jk(&c, &cache).job_builds(&none(), "j", "number", 1).await.unwrap_err().status(), Some(500));
        assert_eq!(count(&c), 3, "three attempts, then the error");
        let c = client(vec![status(404, json!({})), ok(json!({}))]);
        assert_eq!(jk(&c, &cache).job_builds(&none(), "j", "number", 1).await.unwrap_err().status(), Some(404));
        assert_eq!(count(&c), 1);
    }

    #[tokio::test]
    async fn test_reports_are_cached_only_when_asked_and_a_404_is_none_and_never_cached() {
        let report = json!({ "failCount": 1, "passCount": 2, "skipCount": 0, "suites": [] });
        let c = client(vec![ok(report.clone()), status(404, json!({})), ok(report.clone()), ok(report.clone())]);
        let cache = JenkinsCache::default();
        let j = jk(&c, &cache);
        assert_eq!(j.fetch_test_report(&none(), "j", 7, true).await.unwrap(), Some(report.clone()));
        assert_eq!(j.fetch_test_report(&none(), "j", 7, true).await.unwrap(), Some(report.clone()));
        assert_eq!(count(&c), 1, "the second read came from the cache");
        assert_eq!(j.fetch_test_report(&none(), "j", 8, true).await.unwrap(), None);
        assert_eq!(j.fetch_test_report(&none(), "j", 8, true).await.unwrap(), Some(report.clone()), "a 404 wasn't remembered");
        assert_eq!(j.fetch_test_report(&none(), "j", 9, false).await.unwrap(), Some(report));
        assert_eq!(count(&c), 4);
        let before = count(&c);
        assert!(cache.reports.lock().unwrap().get("j#9").is_none(), "an uncacheable read isn't stored");
        cache.clear();
        assert!(cache.reports.lock().unwrap().is_empty());
        assert_eq!(count(&c), before);
        assert!(c.transport.seen.lock().unwrap()[0].url.contains("/job/j/7/testReport/api/json?tree=failCount%2CpassCount%2CskipCount%2Csuites%5Bname%2Ccases%5BclassName"));
    }
}
