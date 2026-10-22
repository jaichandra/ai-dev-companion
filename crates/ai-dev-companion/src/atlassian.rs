//! The shared "how does a feature talk to an internal Atlassian site" helper
//! (port of `core/atlassian.ts`, with `core/auth-context.ts` in
//! `auth_context.rs`). The companion service has no browser session of its own:
//! the extension relays one, so every feature that needs Jira or Bitbucket goes
//! through here rather than reimplementing this fallback chain itself.
//!
//! Auth order (SSO first, by design):
//!   1. The browser session relayed from the page (`X-Relay-Cookie` /
//!      `X-Relay-Origin`). Only ever forwarded to the exact origin it came from.
//!   2. A cached browser session from some other request or the extension's
//!      heartbeat (`auth_context`).
//!   3. A configured API token (`site.api_token`, resolved from the encrypted
//!      credential store, never config.json).
//!   4. Neither works: `AuthSetupError`, whose message is written to be shown
//!      directly to the user (it ends up in `job.error`, which the review panel
//!      renders), pointing them at the Settings panel.

use crate::auth_context::AuthHub;
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Redirect, TransportError};
use crate::js_date::to_iso_string;
use crate::session_vault::origin_of;
use base64::Engine;
use serde_json::Value;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthContext {
    /// "name=value; name2=value2" relayed from the page's own cookies.
    pub cookie: Option<String>,
    /// Origin the cookie was relayed from: the cookie is only ever used against
    /// a site whose base URL has this same origin.
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthScheme {
    /// Jira Server's scheme.
    #[default]
    Bearer,
    /// `username:apiToken` base64-encoded: Jenkins' convention, where the
    /// "password" half is an API token rather than a real password.
    Basic,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteAuth {
    /// e.g. "https://jira.example.com", no trailing slash.
    pub base_url: String,
    /// Human-readable name for error messages, e.g. "Jira".
    pub label: String,
    /// Dotted config path for error messages, e.g. "jira.apiToken".
    pub config_key: String,
    pub username: Option<String>,
    pub api_token: Option<String>,
    pub auth_scheme: AuthScheme,
    /// The site has no browser login the companion can borrow (GitHub's page
    /// cookies don't authenticate its API): a saved token is the only way in, so
    /// the session cookie and vault are never tried and a 403 is read as "not
    /// allowed" or "rate limited", not as "bad token".
    pub token_only: bool,
    /// Extra request headers for every call (GitHub's Accept and API version); they win over the defaults.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtlassianError {
    /// Safe to surface to the user as-is.
    AuthSetup(String),
    /// A non-ok HTTP response, carrying the status so callers can react to a
    /// specific one (Jenkins answers 404 for a build that published no test report).
    Status { message: String, status: u16 },
    /// The request never got an answer.
    Transport(TransportError),
    /// Anything else a client call can fail with (a refused argument, a reply that lacks what it must carry).
    Other(String),
}

impl AtlassianError {
    pub fn status(&self) -> Option<u16> {
        match self {
            AtlassianError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl std::fmt::Display for AtlassianError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtlassianError::AuthSetup(m) | AtlassianError::Status { message: m, .. } | AtlassianError::Other(m) => f.write_str(m),
            AtlassianError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AtlassianError {}

impl From<String> for AtlassianError {
    fn from(m: String) -> Self {
        AtlassianError::Other(m)
    }
}

impl AtlassianError {
    /// A 404 from the site: ordinary "nothing here" for several callers.
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}

impl From<TransportError> for AtlassianError {
    fn from(e: TransportError) -> Self {
        AtlassianError::Transport(e)
    }
}

fn is_redirect_or_auth_failure(res: &HttpResponse) -> bool {
    if res.status == 401 || res.status == 403 || (300..400).contains(&res.status) {
        return true;
    }
    // Jira Server's reliable "you're not actually logged in" signal even on a
    // 200 (some endpoints don't bother with a 401).
    res.header("x-ausername").is_some_and(|v| v.to_lowercase() == "anonymous")
}

/// The Authorization header for `site`'s configured token: Bearer unless the
/// site asked for HTTP Basic. Basic needs a username to pair the token with, so
/// a half-configured site fails here with the same "go fix your config" wording
/// as a missing token.
fn authorization_header(site: &SiteAuth) -> Result<String, AtlassianError> {
    let token = site.api_token.as_deref().unwrap_or("");
    if site.auth_scheme != AuthScheme::Basic {
        return Ok(format!("Bearer {token}"));
    }
    let Some(user) = site.username.as_deref().filter(|u| !u.is_empty()) else {
        return Err(AtlassianError::AuthSetup(format!(
            "{} needs a username alongside its API token. Add the username next to \"{}\" in companion-service/config.json (see config.example.json) and restart the service.",
            site.label, site.config_key
        )));
    };
    Ok(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))))
}

/// A parsed answer plus the headers of whichever attempt (cookie or token)
/// actually answered: for callers that read more than the body (Bitbucket's
/// `whoAmI` reads `X-AUSERNAME`).
#[derive(Debug, Clone)]
pub struct AuthedResponse {
    /// `Null` when the body was empty or not JSON.
    pub body: Value,
    pub headers: Vec<(String, String)>,
}

fn rate_limited(res: &HttpResponse) -> bool {
    res.status == 429 || res.header("x-ratelimit-remaining") == Some("0") || res.header("retry-after").is_some()
}

fn rate_limit_reset(res: &HttpResponse) -> String {
    let num = |h: &str| res.header(h).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
    let (reset, retry_after) = (num("x-ratelimit-reset"), num("retry-after"));
    if retry_after > 0.0 {
        return format!(" (retry in {}s)", crate::js_text::js_to_string(&crate::js_number::json_number(retry_after)));
    }
    if reset > 0.0 {
        if let Some(iso) = to_iso_string((reset * 1000.0) as i64) {
            return format!(" (resets at {} UTC)", &iso[11..16]);
        }
    }
    String::new()
}

fn parse_json_or_throw(res: HttpResponse, site: &SiteAuth) -> Result<AuthedResponse, AtlassianError> {
    let text = res.text();
    let data: Option<Value> = if text.is_empty() { None } else { serde_json::from_str(&text).ok() };
    if !res.ok() {
        let error_messages = data.as_ref().and_then(|d| d["errorMessages"].as_array()).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).filter(|m| !m.is_empty());
        // GitHub's error body: { message, documentation_url }.
        let message_field = data.as_ref().and_then(|d| d["message"].as_str()).filter(|m| !m.is_empty()).map(String::from);
        let message = error_messages.or(message_field).or_else(|| (!text.is_empty()).then(|| text.clone())).unwrap_or_else(|| format!("HTTP {}", res.status));
        return Err(AtlassianError::Status { message: format!("{} request failed: {message}", site.label), status: res.status });
    }
    Ok(AuthedResponse { body: data.unwrap_or(Value::Null), headers: res.headers })
}

fn rejected_token(site: &SiteAuth) -> AtlassianError {
    AtlassianError::AuthSetup(format!("{} rejected the saved API token ({}). Generate a fresh token and save it in \u{2728} \u{2192} \u{2699} Settings.", site.label, site.config_key))
}

/// Fetches JSON from `site.base_url + path_and_query`, trying the relayed
/// browser session first and falling back to an API token. Fails with
/// `AuthSetup` (safe to surface to the user as-is) if neither works.
pub async fn authed_json_with_headers(
    transport: &impl HttpTransport,
    hub: &AuthHub,
    site: &SiteAuth,
    auth: &AuthContext,
    path_and_query: &str,
    method: Option<&str>,
    body: Option<&Value>,
) -> Result<AuthedResponse, AtlassianError> {
    // Try the cached browser session (recorded from some other request, or the
    // extension's heartbeat) before falling back to the token.
    let (site, auth, source) = if site.token_only {
        (site.clone(), auth.clone(), "none")
    } else {
        let r = hub.auth_context_for(site, auth);
        (r.site, r.auth, r.source)
    };
    let url = format!("{}{path_and_query}", site.base_url);
    let method = method.unwrap_or("GET");
    let mut base_headers: Vec<(String, String)> = vec![("Accept".into(), "application/json".into())];
    for (k, v) in &site.headers {
        base_headers.retain(|(bk, _)| !bk.eq_ignore_ascii_case(k));
        base_headers.push((k.clone(), v.clone()));
    }
    let body_text = body.map(Value::to_string);
    if body_text.is_some() {
        base_headers.push(("Content-Type".into(), "application/json".into()));
    }
    let build = |extra: (&str, &str), redirect: Redirect| {
        let mut r = HttpRequest::new(method, &url, REQUEST_TIMEOUT).redirect(redirect);
        for (k, v) in &base_headers {
            r = r.header(k, v);
        }
        r = r.header(extra.0, extra.1);
        match &body_text {
            Some(b) => r.body(b.as_bytes().to_vec()),
            None => r,
        }
    };

    let cookie_usable = auth.cookie.as_deref().filter(|c| !c.is_empty()).filter(|_| auth.origin.is_some() && auth.origin == origin_of(&site.base_url));
    if let Some(cookie) = cookie_usable {
        // A login redirect must not be silently followed into an HTML page and
        // mistaken for a real (empty-ish) JSON response.
        let res = transport.send(build(("Cookie", cookie), Redirect::Manual)).await?;
        if !is_redirect_or_auth_failure(&res) {
            return parse_json_or_throw(res, &site);
        }
        // Falls through to the token attempt below.
    }

    if site.api_token.as_deref().is_some_and(|t| !t.is_empty()) {
        let res = transport.send(build(("Authorization", &authorization_header(&site)?), Redirect::Follow)).await?;
        if site.token_only {
            if res.status == 401 {
                return Err(rejected_token(&site));
            }
            // A 403/429 on a token that works is a rate limit or a missing permission, not a bad token.
            if (res.status == 403 || res.status == 429) && rate_limited(&res) {
                return Err(AtlassianError::Status { message: format!("{} rate limit reached{}. Try again later.", site.label, rate_limit_reset(&res)), status: res.status });
            }
            return parse_json_or_throw(res, &site);
        }
        if is_redirect_or_auth_failure(&res) {
            return Err(rejected_token(&site));
        }
        return parse_json_or_throw(res, &site);
    }

    if site.token_only {
        return Err(AtlassianError::AuthSetup(format!(
            "No {l} API token is saved, and {l} can only be used with one. Create a personal access token and add it in \u{2728} \u{2192} \u{2699} Settings.",
            l = site.label
        )));
    }
    if source == "none" {
        // There was no browser session (request or vault) to even try, so "rejected"
        // would be misleading: a different, more actionable message than the one below
        // (which is for a cookie that was tried and turned out to be stale/invalid).
        return Err(AtlassianError::AuthSetup(format!(
            "No {l} browser session is available to the companion right now (open {l} in Chrome, or turn on \"Keep browser sessions warm\" in \u{2728} \u{2192} \u{2699} Settings), and no API token is saved. Add one in \u{2728} \u{2192} \u{2699} Settings.",
            l = site.label
        )));
    }
    Err(AtlassianError::AuthSetup(format!(
        "{l} rejected the browser session relayed from the extension, and no API token is saved. Add one in \u{2728} \u{2192} \u{2699} Settings \u{2014} or make sure you're logged into {l} in this browser.",
        l = site.label
    )))
}

/// `authed_json_with_headers`, for the majority of callers that never need the response headers.
pub async fn authed_json(transport: &impl HttpTransport, hub: &AuthHub, site: &SiteAuth, auth: &AuthContext, path_and_query: &str, method: Option<&str>, body: Option<&Value>) -> Result<Value, AtlassianError> {
    Ok(authed_json_with_headers(transport, hub, site, auth, path_and_query, method, body).await?.body)
}

/// An HTTP transport plus the auth hub: what every site client talks through.
pub struct Client<T: HttpTransport> {
    pub transport: T,
    pub hub: std::sync::Arc<AuthHub>,
}

impl<T: HttpTransport> Client<T> {
    pub fn new(transport: T, hub: std::sync::Arc<AuthHub>) -> Self {
        Self { transport, hub }
    }

    pub async fn json(&self, site: &SiteAuth, auth: &AuthContext, path_and_query: &str, method: Option<&str>, body: Option<&Value>) -> Result<Value, AtlassianError> {
        authed_json(&self.transport, &self.hub, site, auth, path_and_query, method, body).await
    }

    pub async fn get(&self, site: &SiteAuth, auth: &AuthContext, path_and_query: &str) -> Result<Value, AtlassianError> {
        self.json(site, auth, path_and_query, None, None).await
    }

    pub async fn json_with_headers(&self, site: &SiteAuth, auth: &AuthContext, path_and_query: &str, method: Option<&str>, body: Option<&Value>) -> Result<AuthedResponse, AtlassianError> {
        authed_json_with_headers(&self.transport, &self.hub, site, auth, path_and_query, method, body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::TokenLookup;
    use crate::http::testing::FakeTransport;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Tokens(Vec<(&'static str, &'static str)>);
    impl TokenLookup for Tokens {
        fn get_token(&self, name: &str) -> Option<String> {
            self.0.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
        }
    }

    fn hub(tokens: Vec<(&'static str, &'static str)>) -> AuthHub {
        AuthHub::new(Arc::new(Tokens(tokens)))
    }

    fn jira() -> SiteAuth {
        SiteAuth { base_url: "https://jira.example.com".into(), label: "Jira".into(), config_key: "jira.apiToken".into(), ..Default::default() }
    }

    fn res(status: u16, body: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse { status, headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: body.as_bytes().to_vec() })
    }

    fn fake(answers: Vec<Result<HttpResponse, TransportError>>) -> FakeTransport {
        let q = Mutex::new(VecDeque::from(answers));
        FakeTransport::new(move |_| q.lock().unwrap().pop_front().expect("more calls than answers"))
    }

    fn cookie(c: &str) -> AuthContext {
        AuthContext { cookie: Some(c.into()), origin: Some("https://jira.example.com".into()) }
    }

    async fn get(t: &FakeTransport, h: &AuthHub, site: &SiteAuth, auth: &AuthContext) -> Result<Value, AtlassianError> {
        authed_json(t, h, site, auth, "/rest/api/2/myself", None, None).await
    }

    fn header(t: &FakeTransport, call: usize, name: &str) -> Option<String> {
        t.seen.lock().unwrap()[call].headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }

    #[tokio::test]
    async fn the_relayed_session_is_tried_first_with_a_manual_redirect_and_no_token_is_sent() {
        let t = fake(vec![res(200, r#"{"name":"me"}"#, &[("x-ausername", "me")])]);
        let site = SiteAuth { api_token: Some("tok".into()), ..jira() };
        assert_eq!(get(&t, &hub(vec![]), &site, &cookie("JSESSIONID=1")).await.unwrap(), json!({ "name": "me" }));
        assert_eq!(header(&t, 0, "cookie").as_deref(), Some("JSESSIONID=1"));
        assert_eq!(header(&t, 0, "authorization"), None);
        assert_eq!(t.seen.lock().unwrap()[0].redirect, Redirect::Manual);
        assert_eq!(t.seen.lock().unwrap()[0].url, "https://jira.example.com/rest/api/2/myself");
    }

    #[tokio::test]
    async fn a_session_for_another_origin_is_never_sent_and_the_token_is_used_instead() {
        let t = fake(vec![res(200, "{}", &[])]);
        let other = AuthContext { cookie: Some("S=1".into()), origin: Some("https://evil.example.com".into()) };
        let site = SiteAuth { api_token: Some("tok".into()), ..jira() };
        get(&t, &hub(vec![]), &site, &other).await.unwrap();
        assert_eq!(header(&t, 0, "cookie"), None);
        assert_eq!(header(&t, 0, "authorization").as_deref(), Some("Bearer tok"));
    }

    #[tokio::test]
    async fn a_rejected_or_anonymous_session_falls_back_to_the_token() {
        for first in [res(401, "", &[]), res(302, "", &[("location", "/login")]), res(200, "{}", &[("x-ausername", "ANONYMOUS")])] {
            let t = fake(vec![first, res(200, r#"{"ok":true}"#, &[])]);
            let site = SiteAuth { api_token: Some("tok".into()), ..jira() };
            assert_eq!(get(&t, &hub(vec![]), &site, &cookie("S=1")).await.unwrap(), json!({ "ok": true }));
            assert_eq!(t.seen.lock().unwrap().len(), 2);
            assert_eq!(header(&t, 1, "authorization").as_deref(), Some("Bearer tok"));
            assert_eq!(header(&t, 1, "cookie"), None);
        }
    }

    #[tokio::test]
    async fn the_token_comes_from_the_credential_store_when_the_site_didnt_carry_one() {
        let t = fake(vec![res(200, "{}", &[])]);
        get(&t, &hub(vec![("jira.apiToken", "stored")]), &jira(), &AuthContext::default()).await.unwrap();
        assert_eq!(header(&t, 0, "authorization").as_deref(), Some("Bearer stored"));
    }

    #[tokio::test]
    async fn a_cached_session_from_another_request_is_used_before_the_token() {
        let h = hub(vec![("jira.apiToken", "stored")]);
        h.configure_session_vault(Arc::new(std::sync::RwLock::new(json!({ "jira": { "baseUrl": "https://jira.example.com" } }))));
        h.record_relayed_session(&cookie("S=cached"));
        let t = fake(vec![res(200, "{}", &[])]);
        get(&t, &h, &jira(), &AuthContext::default()).await.unwrap();
        assert_eq!(header(&t, 0, "cookie").as_deref(), Some("S=cached"));
        assert_eq!(header(&t, 0, "authorization"), None);
    }

    #[tokio::test]
    async fn a_rejected_token_names_the_config_key_and_points_at_settings() {
        let t = fake(vec![res(401, "", &[])]);
        let e = get(&t, &hub(vec![("jira.apiToken", "bad")]), &jira(), &AuthContext::default()).await.unwrap_err();
        assert!(matches!(&e, AtlassianError::AuthSetup(m) if m.starts_with("Jira rejected the saved API token (jira.apiToken).") && m.contains("Settings")), "{e}");
    }

    #[tokio::test]
    async fn with_no_session_and_no_token_the_error_says_what_to_do_and_with_a_stale_session_it_says_so() {
        let t = fake(vec![]);
        let e = get(&t, &hub(vec![]), &jira(), &AuthContext::default()).await.unwrap_err();
        assert!(e.to_string().starts_with("No Jira browser session is available to the companion right now"), "{e}");
        assert!(t.seen.lock().unwrap().is_empty());
        let t = fake(vec![res(403, "", &[])]);
        let e = get(&t, &hub(vec![]), &jira(), &cookie("S=1")).await.unwrap_err();
        assert!(e.to_string().starts_with("Jira rejected the browser session relayed from the extension"), "{e}");
    }

    #[tokio::test]
    async fn basic_auth_pairs_the_token_with_the_username_and_refuses_without_one() {
        let t = fake(vec![res(200, "{}", &[])]);
        let site = SiteAuth { api_token: Some("tok".into()), username: Some("jdoe".into()), auth_scheme: AuthScheme::Basic, label: "Jenkins".into(), config_key: "jenkins.apiToken".into(), base_url: "https://jenkins.example.com".into(), ..Default::default() };
        get(&t, &hub(vec![]), &site, &AuthContext::default()).await.unwrap();
        assert_eq!(header(&t, 0, "authorization").as_deref(), Some("Basic am9lOnRvaw==".replace("am9lOnRvaw==", &base64::engine::general_purpose::STANDARD.encode("jdoe:tok")).as_str()));
        let no_user = SiteAuth { username: None, ..site };
        let e = get(&fake(vec![]), &hub(vec![]), &no_user, &AuthContext::default()).await.unwrap_err();
        assert!(e.to_string().starts_with("Jenkins needs a username alongside its API token."), "{e}");
    }

    #[tokio::test]
    async fn token_only_sites_skip_sessions_and_read_403_and_429_as_limits() {
        let gh = SiteAuth { base_url: "https://api.github.com".into(), label: "GitHub".into(), config_key: "github.apiToken".into(), token_only: true, api_token: Some("tok".into()), headers: vec![("Accept".into(), "application/vnd.github+json".into())], ..Default::default() };
        let t = fake(vec![res(200, "{}", &[])]);
        get(&t, &hub(vec![]), &gh, &cookie("S=1")).await.unwrap();
        assert_eq!(header(&t, 0, "cookie"), None, "no session is ever tried");
        assert_eq!(header(&t, 0, "accept").as_deref(), Some("application/vnd.github+json"), "site headers win");
        let limited = fake(vec![res(403, "{}", &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1790647200")])]);
        let e = get(&limited, &hub(vec![]), &gh, &AuthContext::default()).await.unwrap_err();
        assert_eq!(e, AtlassianError::Status { message: "GitHub rate limit reached (resets at 02:00 UTC). Try again later.".into(), status: 403 });
        let secondary = fake(vec![res(429, "{}", &[("retry-after", "30")])]);
        assert!(get(&secondary, &hub(vec![]), &gh, &AuthContext::default()).await.unwrap_err().to_string().contains("(retry in 30s)"));
        let forbidden = fake(vec![res(403, r#"{"message":"Resource not accessible"}"#, &[])]);
        assert_eq!(get(&forbidden, &hub(vec![]), &gh, &AuthContext::default()).await.unwrap_err().status(), Some(403));
        assert!(get(&fake(vec![res(401, "", &[])]), &hub(vec![]), &gh, &AuthContext::default()).await.unwrap_err().to_string().contains("rejected the saved API token (github.apiToken)"));
        let none = SiteAuth { api_token: None, ..gh };
        assert!(get(&fake(vec![]), &hub(vec![]), &none, &AuthContext::default()).await.unwrap_err().to_string().starts_with("No GitHub API token is saved, and GitHub can only be used with one."));
    }

    #[tokio::test]
    async fn error_bodies_are_read_for_jira_github_or_plain_text_and_an_empty_body_is_null() {
        let site = SiteAuth { api_token: Some("t".into()), ..jira() };
        let h = hub(vec![]);
        let msg = |r| async move { get(&fake(vec![r]), &hub(vec![]), &site_of(), &AuthContext::default()).await.unwrap_err() };
        fn site_of() -> SiteAuth {
            SiteAuth { api_token: Some("t".into()), ..jira() }
        }
        let _ = (&site, &h);
        let e = msg(res(400, r#"{"errorMessages":["a","b"],"errors":{}}"#, &[])).await;
        assert_eq!(e, AtlassianError::Status { message: "Jira request failed: a; b".into(), status: 400 });
        assert_eq!(msg(res(404, r#"{"message":"Not Found"}"#, &[])).await.to_string(), "Jira request failed: Not Found");
        assert_eq!(msg(res(500, "boom", &[])).await.to_string(), "Jira request failed: boom");
        assert_eq!(msg(res(500, "", &[])).await.to_string(), "Jira request failed: HTTP 500");
        assert_eq!(msg(res(404, r#"{"errorMessages":[]}"#, &[])).await.to_string(), "Jira request failed: {\"errorMessages\":[]}");
        let ok = get(&fake(vec![res(204, "", &[])]), &hub(vec![]), &site_of(), &AuthContext::default()).await.unwrap();
        assert_eq!(ok, Value::Null);
    }

    #[tokio::test]
    async fn a_post_sends_its_json_body_and_content_type_and_the_response_headers_come_back() {
        let t = fake(vec![res(200, r#"{"id":1}"#, &[("x-ausername", "me")])]);
        let site = SiteAuth { api_token: Some("tok".into()), ..jira() };
        let r = authed_json_with_headers(&t, &hub(vec![]), &site, &AuthContext::default(), "/rest/api/2/issue", Some("POST"), Some(&json!({ "a": 1 }))).await.unwrap();
        assert_eq!(r.body, json!({ "id": 1 }));
        assert!(r.headers.contains(&("x-ausername".to_string(), "me".to_string())));
        let seen = t.seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].body.as_deref()), ("POST", Some(br#"{"a":1}"#.as_slice())));
        drop(seen);
        assert_eq!(header(&t, 0, "content-type").as_deref(), Some("application/json"));
    }

    #[tokio::test]
    async fn a_transport_failure_is_reported_as_such() {
        let e = get(&fake(vec![Err(TransportError::ConnectionRefused)]), &hub(vec![("jira.apiToken", "t")]), &jira(), &AuthContext::default()).await.unwrap_err();
        assert_eq!(e, AtlassianError::Transport(TransportError::ConnectionRefused));
    }
}
