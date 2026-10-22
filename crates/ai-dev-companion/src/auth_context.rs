//! Wires `session_vault`'s in-memory cache of relayed browser sessions into
//! every feature's auth path (port of `core/auth-context.ts`): the hub tries the
//! request's own relayed cookie, then the vault (a session relayed by some
//! *other* request, or by the extension's heartbeat), then leaves the token
//! fallback to `atlassian::authed_json_with_headers` exactly as before.

use crate::atlassian::{AuthContext, SiteAuth};
use crate::credentials::TokenLookup;
use crate::environment;
use crate::session_vault::{origin_of, SessionVault};
use serde_json::Value;
use std::sync::{Arc, Mutex, RwLock};

/// The config the vault reads live (a `sessionCache` change saved from the
/// Settings panel takes effect on the very next request, no restart).
pub type SharedConfig = Arc<RwLock<Value>>;

/// The origins any Jira/Jenkins/Bitbucket call could need a relayed cookie for:
/// the only origins the vault will ever accept a cookie against (the relay origin
/// comes from the client and isn't trusted otherwise). De-duplicated: nothing
/// stops two of the sites sharing a host. A token-only site (GitHub) has no
/// browser session worth relaying, so its origin is never accepted for one.
pub fn configured_origins(config: &Value) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for site in environment::current().sites.iter().filter(|s| s.token_only != Some(true)) {
        let base = config[&site.id]["baseUrl"].as_str().filter(|b| !b.is_empty()).unwrap_or(&site.base_url);
        if let Some(o) = origin_of(base) {
            if !out.contains(&o) {
                out.push(o);
            }
        }
    }
    out
}

/// How long (ms) a cached cookie stays usable. 0 (or below) turns caching off.
pub fn session_ttl_ms(config: &Value) -> i64 {
    config["sessionCache"]["ttlMinutes"].as_i64().unwrap_or(30) * 60_000
}

/// Which origins the extension should proactively keep a fresh cookie relayed
/// for, even without a matching feature call: only when the panel turned the
/// heartbeat on AND the cache is actually able to hold anything (ttl > 0).
pub fn heartbeat_hosts(config: &Value) -> (bool, Vec<String>) {
    let on = config["sessionCache"]["heartbeat"] == Value::Bool(true) && session_ttl_ms(config) > 0;
    (on, if on { configured_origins(config) } else { vec![] })
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub auth: AuthContext,
    pub site: SiteAuth,
    /// "request" | "vault" | "none"
    pub source: &'static str,
}

pub struct AuthHub {
    /// `None` until `configure_session_vault` runs. doctor, setup and every test
    /// build a hub without ever configuring it, so they never cache a cookie.
    vault: Mutex<Option<SessionVault>>,
    tokens: Arc<dyn TokenLookup>,
}

impl AuthHub {
    pub fn new(tokens: Arc<dyn TokenLookup>) -> Self {
        Self { vault: Mutex::new(None), tokens }
    }

    /// Builds the vault, called once from the server right after the features are built.
    pub fn configure_session_vault(&self, config: SharedConfig) {
        let (a, b) = (config.clone(), config);
        *self.vault.lock().unwrap() = Some(SessionVault::new(move || session_ttl_ms(&a.read().unwrap()), move || configured_origins(&b.read().unwrap())));
    }

    /// Caches a relayed browser session so a later request that carries no cookie
    /// of its own (an MCP tool call from terminal Claude, or a feature call for a
    /// site the current page isn't open to) can still act as the logged-in user
    /// for a while. A no-op until `configure_session_vault` has run, or when `auth`
    /// is missing either field.
    pub fn record_relayed_session(&self, auth: &AuthContext) {
        let (Some(cookie), Some(origin)) = (auth.cookie.as_deref().filter(|c| !c.is_empty()), auth.origin.as_deref().filter(|o| !o.is_empty())) else { return };
        if let Some(v) = self.vault.lock().unwrap().as_mut() {
            v.record(origin, cookie);
        }
    }

    /// The auth to actually use for `site`, trying in order: the request's own
    /// relayed cookie (only if it matches the site's origin), the vault's cached
    /// cookie for that origin, nothing (the caller falls back to `site.api_token`).
    /// `site.api_token` is filled in from the credential store only when the
    /// caller left it unset.
    pub fn auth_context_for(&self, site: &SiteAuth, auth: &AuthContext) -> Resolved {
        let mut with_token = site.clone();
        if with_token.api_token.is_none() {
            with_token.api_token = self.tokens.get_token(&site.config_key);
        }
        let Some(origin) = origin_of(&site.base_url) else { return Resolved { auth: AuthContext::default(), site: with_token, source: "none" } };
        let from_request = auth.cookie.clone().filter(|c| !c.is_empty() && auth.origin.as_deref() == Some(origin.as_str()));
        let from_vault = || self.vault.lock().unwrap().as_mut().and_then(|v| v.get(&origin));
        let (cookie, source) = match from_request {
            Some(c) => (Some(c), "request"),
            None => match from_vault() {
                Some(c) => (Some(c), "vault"),
                None => (None, "none"),
            },
        };
        Resolved { auth: cookie.map_or_else(AuthContext::default, |c| AuthContext { cookie: Some(c), origin: Some(origin) }), site: with_token, source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct NoTokens;
    impl TokenLookup for NoTokens {
        fn get_token(&self, name: &str) -> Option<String> {
            (name == "jira.apiToken").then(|| "stored".to_string())
        }
    }

    fn hub() -> AuthHub {
        AuthHub::new(Arc::new(NoTokens))
    }

    fn jira() -> SiteAuth {
        SiteAuth { base_url: "https://jira.example.com".into(), label: "Jira".into(), config_key: "jira.apiToken".into(), ..Default::default() }
    }

    fn shared(v: Value) -> SharedConfig {
        Arc::new(RwLock::new(v))
    }

    #[test]
    fn configured_origins_follow_config_dedupe_and_leave_out_token_only_sites() {
        let defaults = configured_origins(&json!({}));
        assert_eq!(defaults, ["https://jira.example.com", "https://jenkins.example.com"], "the placeholder's GitHub is token-only");
        let same = configured_origins(&json!({ "jira": { "baseUrl": "https://x.example.com/a" }, "jenkins": { "baseUrl": "https://x.example.com" } }));
        assert_eq!(same, ["https://x.example.com"]);
        assert!(configured_origins(&json!({ "jira": { "baseUrl": "ftp://nope" }, "jenkins": { "baseUrl": "junk" } })).is_empty());
    }

    #[test]
    fn the_session_ttl_and_the_heartbeat_read_the_session_cache_settings() {
        assert_eq!(session_ttl_ms(&json!({})), 30 * 60_000);
        assert_eq!(session_ttl_ms(&json!({ "sessionCache": { "ttlMinutes": 0 } })), 0);
        assert_eq!(heartbeat_hosts(&json!({})), (false, vec![]));
        assert_eq!(heartbeat_hosts(&json!({ "sessionCache": { "heartbeat": true } })).1.len(), 2);
        assert_eq!(heartbeat_hosts(&json!({ "sessionCache": { "heartbeat": true, "ttlMinutes": 0 } })), (false, vec![]), "a heartbeat feeding an off vault is pointless");
    }

    #[test]
    fn the_requests_own_cookie_wins_only_for_its_own_origin_and_the_token_is_filled_in() {
        let h = hub();
        let mine = AuthContext { cookie: Some("S=1".into()), origin: Some("https://jira.example.com".into()) };
        let r = h.auth_context_for(&jira(), &mine);
        assert_eq!((r.source, r.auth.cookie.as_deref(), r.site.api_token.as_deref()), ("request", Some("S=1"), Some("stored")));
        let other = AuthContext { cookie: Some("S=1".into()), origin: Some("https://evil.example.com".into()) };
        let r = h.auth_context_for(&jira(), &other);
        assert_eq!((r.source, r.auth), ("none", AuthContext::default()));
        let kept = SiteAuth { api_token: Some("given".into()), ..jira() };
        assert_eq!(h.auth_context_for(&kept, &mine).site.api_token.as_deref(), Some("given"));
        let bad = SiteAuth { base_url: "not a url".into(), ..jira() };
        assert_eq!(h.auth_context_for(&bad, &mine).source, "none");
    }

    #[test]
    fn a_recorded_session_is_found_later_but_only_after_the_vault_is_configured_and_for_allowed_origins() {
        let h = hub();
        let session = AuthContext { cookie: Some("S=cached".into()), origin: Some("https://jira.example.com".into()) };
        h.record_relayed_session(&session);
        assert_eq!(h.auth_context_for(&jira(), &AuthContext::default()).source, "none", "nothing cached before it is configured");
        let config = shared(json!({}));
        h.configure_session_vault(config.clone());
        h.record_relayed_session(&AuthContext { cookie: Some("S=evil".into()), origin: Some("https://evil.example.com".into()) });
        h.record_relayed_session(&AuthContext { cookie: Some("".into()), origin: Some("https://jira.example.com".into()) });
        h.record_relayed_session(&session);
        let r = h.auth_context_for(&jira(), &AuthContext::default());
        assert_eq!((r.source, r.auth.cookie.as_deref()), ("vault", Some("S=cached")));
        config.write().unwrap()["sessionCache"] = json!({ "ttlMinutes": 0 });
        assert_eq!(h.auth_context_for(&jira(), &AuthContext::default()).source, "none", "a Settings change applies on the very next request");
    }
}
