//! In-memory cache of the browser sessions the extension relays
//! (`X-Relay-Cookie` / `X-Relay-Origin`), so a request that carries no cookie
//! of its own, such as an MCP tool call from terminal Claude, can still act as
//! the logged-in user for a while (port of `core/session-vault.js`). Never
//! logged, never written anywhere: the map below is the only copy, and neither
//! `Debug` nor `Serialize` can reach a cookie or an origin.

use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;
use url::Url;

pub const MAX_COOKIE_LENGTH: usize = 16 * 1024;

/// `http(s)` origin of a base URL (no path, no trailing slash), else `None`.
pub fn origin_of(base_url: &str) -> Option<String> {
    let url = Url::parse(base_url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

fn is_usable_cookie(cookie: &str) -> bool {
    // `length` is UTF-16 units in JS.
    !cookie.is_empty() && cookie.encode_utf16().count() <= MAX_COOKIE_LENGTH && !cookie.contains(['\r', '\n'])
}

struct Entry {
    cookie: String,
    at: i64,
}

pub struct SessionVault {
    entries: HashMap<String, Entry>,
    /// Read live on every call, so a Settings change applies without a restart.
    ttl_ms: Box<dyn Fn() -> i64 + Send + Sync>,
    allowed_origins: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl SessionVault {
    pub fn new(
        ttl_ms: impl Fn() -> i64 + Send + Sync + 'static,
        allowed_origins: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self::with_clock(ttl_ms, allowed_origins, now_ms)
    }

    pub fn with_clock(
        ttl_ms: impl Fn() -> i64 + Send + Sync + 'static,
        allowed_origins: impl Fn() -> Vec<String> + Send + Sync + 'static,
        clock: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        SessionVault {
            entries: HashMap::new(),
            ttl_ms: Box::new(ttl_ms),
            allowed_origins: Box::new(allowed_origins),
            clock: Box::new(clock),
        }
    }

    pub fn record(&mut self, origin: &str, cookie: &str) -> bool {
        if (self.ttl_ms)() <= 0 || origin.is_empty() || !is_usable_cookie(cookie) {
            return false;
        }
        // Exact origin match only: X-Relay-Origin is client-supplied.
        if !(self.allowed_origins)().iter().any(|o| o == origin) {
            return false;
        }
        self.entries.insert(origin.to_string(), Entry { cookie: cookie.to_string(), at: (self.clock)() });
        true
    }

    pub fn get(&mut self, origin: &str) -> Option<String> {
        let entry = self.entries.get(origin)?;
        let ttl = (self.ttl_ms)();
        if ttl <= 0 || (self.clock)() - entry.at > ttl || !(self.allowed_origins)().iter().any(|o| o == origin) {
            self.entries.remove(origin);
            return None;
        }
        Some(entry.cookie.clone())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }
}

impl fmt::Debug for SessionVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionVault {{ entries: {} }}", self.entries.len())
    }
}

impl Serialize for SessionVault {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("SessionVault", 1)?;
        st.serialize_field("entries", &self.entries.len())?;
        st.end()
    }
}

/// What the request itself carried.
#[derive(Debug, Default, Clone)]
pub struct RequestAuth {
    pub cookie: Option<String>,
    pub origin: Option<String>,
}

/// The request's own cookie when it was issued for this site's origin, else
/// whatever the vault holds for it.
pub fn pick_cookie(site_origin: &str, request_auth: Option<&RequestAuth>, vault: &mut SessionVault) -> Option<String> {
    if let Some(RequestAuth { cookie: Some(c), origin: Some(o) }) = request_auth {
        if !c.is_empty() && o == site_origin {
            return Some(c.clone());
        }
    }
    vault.get(site_origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    const ORIGIN: &str = "https://jira.example.com";

    struct Fixture {
        vault: SessionVault,
        clock: Arc<AtomicI64>,
        ttl: Arc<AtomicI64>,
        origins: Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn fixture(ttl_ms: i64) -> Fixture {
        let clock = Arc::new(AtomicI64::new(0));
        let ttl = Arc::new(AtomicI64::new(ttl_ms));
        let origins = Arc::new(std::sync::Mutex::new(vec![ORIGIN.to_string()]));
        let (c, t, o) = (clock.clone(), ttl.clone(), origins.clone());
        let vault = SessionVault::with_clock(
            move || t.load(Ordering::SeqCst),
            move || o.lock().unwrap().clone(),
            move || c.load(Ordering::SeqCst),
        );
        Fixture { vault, clock, ttl, origins }
    }

    const THIRTY_MIN: i64 = 30 * 60_000;

    #[test]
    fn record_then_get_for_an_allowed_origin_returns_the_cookie() {
        let mut f = fixture(THIRTY_MIN);
        assert!(f.vault.record(ORIGIN, "session=abc"));
        assert_eq!(f.vault.get(ORIGIN).as_deref(), Some("session=abc"));
    }

    #[test]
    fn record_for_a_disallowed_origin_returns_false_and_get_returns_none() {
        for origin in ["https://jira.example.com.evil.test", "http://jira.example.com", "https://jira.example.com:8443", ""] {
            let mut f = fixture(THIRTY_MIN);
            assert!(!f.vault.record(origin, "session=abc"), "{origin:?}");
            assert_eq!(f.vault.get(origin), None, "{origin:?}");
            assert_eq!(f.vault.size(), 0);
        }
    }

    #[test]
    fn the_cookie_must_be_non_empty_and_free_of_line_breaks_and_at_most_16_kb() {
        let mut f = fixture(THIRTY_MIN);
        assert!(!f.vault.record(ORIGIN, ""));
        for bad in ["session=abc\r\nX-Injected: yes", "session=abc\ninjected"] {
            assert!(!f.vault.record(ORIGIN, bad), "{bad:?}");
        }
        assert_eq!(f.vault.size(), 0);
        assert!(!f.vault.record(ORIGIN, &"a".repeat(16 * 1024 + 1)));
        assert!(f.vault.record(ORIGIN, &"a".repeat(16 * 1024)));
        // 16 * 1024 + 1 UTF-16 units in only 8193 chars.
        assert!(!f.vault.record(ORIGIN, &"\u{1F600}".repeat(8 * 1024 + 1)));
    }

    #[test]
    fn ttl_get_at_29_minutes_returns_the_cookie_at_31_returns_none_and_deletes_the_entry() {
        let mut f = fixture(THIRTY_MIN);
        assert!(f.vault.record(ORIGIN, "session=abc"));
        f.clock.fetch_add(29 * 60_000, Ordering::SeqCst);
        assert_eq!(f.vault.get(ORIGIN).as_deref(), Some("session=abc"));
        assert_eq!(f.vault.size(), 1);
        f.clock.fetch_add(2 * 60_000, Ordering::SeqCst);
        assert_eq!(f.vault.get(ORIGIN), None);
        assert_eq!(f.vault.size(), 0, "the expired entry is deleted");
    }

    #[test]
    fn a_later_record_refreshes_the_timestamp() {
        let mut f = fixture(THIRTY_MIN);
        f.vault.record(ORIGIN, "session=abc");
        f.clock.fetch_add(29 * 60_000, Ordering::SeqCst);
        f.vault.record(ORIGIN, "session=def");
        f.clock.fetch_add(29 * 60_000, Ordering::SeqCst);
        assert_eq!(f.vault.get(ORIGIN).as_deref(), Some("session=def"));
    }

    #[test]
    fn a_zero_ttl_stores_nothing_and_a_ttl_dropped_to_zero_hides_what_was_stored() {
        let mut f = fixture(0);
        assert!(!f.vault.record(ORIGIN, "session=abc"));
        assert_eq!((f.vault.get(ORIGIN), f.vault.size()), (None, 0));
        let mut f = fixture(THIRTY_MIN);
        assert!(f.vault.record(ORIGIN, "session=abc"));
        f.ttl.store(0, Ordering::SeqCst);
        assert_eq!(f.vault.get(ORIGIN), None);
    }

    #[test]
    fn the_allowlist_and_the_ttl_are_read_live_on_every_call() {
        let mut f = fixture(THIRTY_MIN);
        assert!(f.vault.record(ORIGIN, "session=abc"));
        f.origins.lock().unwrap().clear(); // Settings removed this origin
        assert_eq!(f.vault.get(ORIGIN), None);

        let mut f = fixture(THIRTY_MIN);
        f.vault.record(ORIGIN, "session=abc");
        f.clock.fetch_add(10 * 60_000, Ordering::SeqCst);
        f.ttl.store(5 * 60_000, Ordering::SeqCst); // Settings shortened the TTL below the age
        assert_eq!(f.vault.get(ORIGIN), None);
    }

    #[test]
    fn neither_serialization_nor_debug_output_contains_the_cookie_or_the_origin() {
        let mut f = fixture(THIRTY_MIN);
        f.vault.record(ORIGIN, "super-secret-session-cookie");
        let json = serde_json::to_string(&f.vault).unwrap();
        assert_eq!(json, r#"{"entries":1}"#);
        let debug = format!("{:?}", f.vault);
        assert_eq!(debug, "SessionVault { entries: 1 }");
        assert!(!format!("{debug}{json}").contains("super-secret"));
        f.vault.clear();
        assert_eq!(f.vault.size(), 0);
    }

    #[test]
    fn pick_cookie_prefers_the_requests_own_cookie_for_the_same_origin_else_the_vault() {
        let mut f = fixture(THIRTY_MIN);
        f.vault.record(ORIGIN, "vault-cookie");
        let own = RequestAuth { cookie: Some("request-cookie".into()), origin: Some(ORIGIN.into()) };
        assert_eq!(pick_cookie(ORIGIN, Some(&own), &mut f.vault).as_deref(), Some("request-cookie"));
        assert_eq!(pick_cookie(ORIGIN, None, &mut f.vault).as_deref(), Some("vault-cookie"));
        assert_eq!(pick_cookie(ORIGIN, Some(&RequestAuth::default()), &mut f.vault).as_deref(), Some("vault-cookie"));
        let other = RequestAuth { cookie: Some("other".into()), origin: Some("https://other.example.com".into()) };
        assert_eq!(pick_cookie(ORIGIN, Some(&other), &mut f.vault).as_deref(), Some("vault-cookie"));
        let mut empty = fixture(THIRTY_MIN);
        assert_eq!(pick_cookie(ORIGIN, Some(&other), &mut empty.vault), None);
    }

    #[test]
    fn origin_of_strips_paths_and_trailing_slashes_and_rejects_non_urls() {
        assert_eq!(origin_of("https://jira.example.com/").as_deref(), Some("https://jira.example.com"));
        assert_eq!(origin_of("https://x.test/sub/path").as_deref(), Some("https://x.test"));
        assert_eq!(origin_of("https://x.test:8443/a").as_deref(), Some("https://x.test:8443"));
        assert_eq!(origin_of("https://x.test:443/a").as_deref(), Some("https://x.test"));
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of("ftp://x.test"), None);
    }
}
