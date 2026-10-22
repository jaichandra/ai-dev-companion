//! The stateful half of the credential wiring (port of `core/token-cache.js`):
//! a short read cache, a "warn once, not on every call" latch, and the
//! undecryptable-store fallbacks, kept apart from the store itself so this
//! logic is testable with a fake store.

use std::collections::HashMap;

const CACHE_TTL_MS: i64 = 60_000;

/// What a credential store can fail with. `Undecryptable` (wrong or missing
/// key, tampering) is treated as "nothing saved"; anything else is a real
/// failure and is passed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Undecryptable(String),
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Undecryptable(m) | StoreError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The credential store as the cache sees it (`get`, `set`, `remove`, `list`).
pub trait CredentialBackend {
    fn get(&self, name: &str) -> Result<Option<String>, StoreError>;
    fn set(&self, name: &str, value: &str) -> Result<(), StoreError>;
    fn remove(&self, name: &str) -> Result<(), StoreError>;
    fn list(&self) -> Result<Vec<String>, StoreError>;
}

struct Entry {
    value: Option<String>,
    expires_at: i64,
}

/// Wraps a store with:
///  - `get_token`: `store.get`, cached per name for 60 seconds.
///  - `set_token`: `store.set`/`store.remove`, immediately refreshing the cache
///    entry so a Settings save takes effect on the very next `get_token`.
///  - `list_names`: `store.list`.
///
/// All three treat an undecryptable store as "nothing saved" rather than a hard
/// failure: `get_token` gives `None`, `list_names` gives `[]`, and clearing a
/// token is a no-op (there is nothing to remove from a file that can't be
/// read). The warning is logged at most once per cache (shared by all three),
/// and only a successful `store.set` (someone re-entered a token) re-arms it.
/// A swallowed `store.remove` failure does not: nothing about the file changed.
pub struct TokenCache<S> {
    store: S,
    cache: HashMap<String, Entry>,
    warned: bool,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
    warn: Box<dyn Fn(&str) + Send + Sync>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl<S: CredentialBackend> TokenCache<S> {
    pub fn new(store: S) -> Self {
        Self::with(store, now_ms, |m| eprintln!("{m}"))
    }

    pub fn with(
        store: S,
        clock: impl Fn() -> i64 + Send + Sync + 'static,
        warn: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        TokenCache { store, cache: HashMap::new(), warned: false, clock: Box::new(clock), warn: Box::new(warn) }
    }

    fn warn_once(&mut self, message: &str) {
        if !self.warned {
            (self.warn)(&format!("[credentials] {message}"));
            self.warned = true;
        }
    }

    fn cache_set(&mut self, name: &str, value: Option<String>) {
        let expires_at = (self.clock)() + CACHE_TTL_MS;
        self.cache.insert(name.to_string(), Entry { value, expires_at });
    }

    pub fn get_token(&mut self, name: &str) -> Result<Option<String>, StoreError> {
        let now = (self.clock)();
        if let Some(c) = self.cache.get(name) {
            if c.expires_at > now {
                return Ok(c.value.clone());
            }
        }
        let value = match self.store.get(name) {
            Ok(v) => v,
            Err(StoreError::Undecryptable(m)) => {
                self.warn_once(&m);
                None
            }
            Err(e) => return Err(e),
        };
        self.cache.insert(name.to_string(), Entry { value: value.clone(), expires_at: now + CACHE_TTL_MS });
        Ok(value)
    }

    /// `Some(value)` saves it; `None` clears it.
    pub fn set_token(&mut self, name: &str, value: Option<&str>) -> Result<(), StoreError> {
        match value {
            None => match self.store.remove(name) {
                Ok(()) => {}
                // Nothing to remove from a file that can't be read: already
                // gone, as far as anything downstream can tell.
                Err(StoreError::Undecryptable(m)) => self.warn_once(&m),
                Err(e) => return Err(e),
            },
            Some(v) => {
                self.store.set(name, v)?;
                // A successful set is how someone recovers from a corrupt
                // store: give the next read a fair try instead of leaving it
                // suppressed by a stale warning.
                self.warned = false;
            }
        }
        self.cache_set(name, value.map(str::to_string));
        Ok(())
    }

    pub fn list_names(&mut self) -> Result<Vec<String>, StoreError> {
        match self.store.list() {
            Ok(names) => Ok(names),
            Err(StoreError::Undecryptable(m)) => {
                self.warn_once(&m);
                Ok(vec![])
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    const UNDECRYPTABLE: &str = "Saved credentials can't be decrypted \u{2014} re-enter your tokens in \u{2728} \u{2192} \u{2699} Settings.";

    #[derive(Clone, PartialEq)]
    enum Mode {
        Ok,
        Broken,
        Failing(String),
    }

    #[derive(Default)]
    struct Calls {
        get: Cell<u32>,
        set: Cell<u32>,
        remove: Cell<u32>,
        list: Cell<u32>,
    }

    /// An in-memory store with call counters, switchable to broken (reads
    /// throw undecryptable) or failing (some other error) mid-test.
    #[derive(Clone)]
    struct FakeStore {
        map: Rc<RefCell<BTreeMap<String, String>>>,
        mode: Rc<RefCell<Mode>>,
        calls: Rc<Calls>,
    }

    impl FakeStore {
        fn new(initial: &[(&str, &str)]) -> Self {
            FakeStore {
                map: Rc::new(RefCell::new(initial.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())),
                mode: Rc::new(RefCell::new(Mode::Ok)),
                calls: Rc::new(Calls::default()),
            }
        }
        fn mode(&self, m: Mode) {
            *self.mode.borrow_mut() = m;
        }
        fn check(&self) -> Result<(), StoreError> {
            match &*self.mode.borrow() {
                Mode::Ok => Ok(()),
                Mode::Broken => Err(StoreError::Undecryptable(UNDECRYPTABLE.into())),
                Mode::Failing(m) => Err(StoreError::Other(m.clone())),
            }
        }
    }

    impl CredentialBackend for FakeStore {
        fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
            self.calls.get.set(self.calls.get.get() + 1);
            self.check()?;
            Ok(self.map.borrow().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
            self.calls.set.set(self.calls.set.get() + 1);
            // set() never throws undecryptable, by design.
            if let Mode::Failing(m) = &*self.mode.borrow() {
                return Err(StoreError::Other(m.clone()));
            }
            self.map.borrow_mut().insert(name.into(), value.into());
            Ok(())
        }
        fn remove(&self, name: &str) -> Result<(), StoreError> {
            self.calls.remove.set(self.calls.remove.get() + 1);
            self.check()?;
            self.map.borrow_mut().remove(name);
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>, StoreError> {
            self.calls.list.set(self.calls.list.get() + 1);
            self.check()?;
            Ok(self.map.borrow().keys().cloned().collect())
        }
    }

    struct Harness {
        cache: TokenCache<FakeStore>,
        store: FakeStore,
        clock: Arc<AtomicI64>,
        warnings: Arc<Mutex<Vec<String>>>,
    }

    fn harness(initial: &[(&str, &str)]) -> Harness {
        let store = FakeStore::new(initial);
        let clock = Arc::new(AtomicI64::new(0));
        let warnings = Arc::new(Mutex::new(vec![]));
        let (c, w) = (clock.clone(), warnings.clone());
        let cache = TokenCache::with(store.clone(), move || c.load(Ordering::SeqCst), move |m| w.lock().unwrap().push(m.to_string()));
        Harness { cache, store, clock, warnings }
    }

    // TokenCache is used behind a lock in the service, and the fake store is
    // single-threaded by design; the Send bounds only apply to the callbacks.

    #[test]
    fn get_token_caches_a_hit_for_60_seconds_without_calling_the_store_again() {
        let mut h = harness(&[("jira.apiToken", "tok")]);
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap().as_deref(), Some("tok"));
        assert_eq!(h.store.calls.get.get(), 1);
        h.clock.fetch_add(59_999, Ordering::SeqCst);
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap().as_deref(), Some("tok"));
        assert_eq!(h.store.calls.get.get(), 1, "still within the 60s window");
    }

    #[test]
    fn get_token_re_reads_the_store_once_the_60_second_entry_expires() {
        let mut h = harness(&[("jira.apiToken", "tok")]);
        h.cache.get_token("jira.apiToken").unwrap();
        h.clock.fetch_add(60_000, Ordering::SeqCst);
        h.store.set("jira.apiToken", "rotated").unwrap();
        h.cache.get_token("jira.apiToken").unwrap();
        assert_eq!(h.store.calls.get.get(), 2);
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap().as_deref(), Some("rotated"));
    }

    #[test]
    fn set_token_immediately_refreshes_the_cache() {
        let mut h = harness(&[("jira.apiToken", "old")]);
        h.cache.get_token("jira.apiToken").unwrap();
        h.cache.set_token("jira.apiToken", Some("new")).unwrap();
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap().as_deref(), Some("new"));
        assert_eq!(h.store.calls.get.get(), 1, "the refreshed entry answered; the store was not read again");
    }

    #[test]
    fn clearing_a_token_removes_it_and_the_cache_reflects_that_immediately() {
        let mut h = harness(&[("jira.apiToken", "tok")]);
        h.cache.set_token("jira.apiToken", None).unwrap();
        assert_eq!(h.store.calls.remove.get(), 1);
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap(), None);
        assert_eq!(h.store.calls.get.get(), 0, "answered from cache, not the store");
    }

    #[test]
    fn get_token_on_an_undecryptable_store_returns_none_and_warns_exactly_once() {
        let mut h = harness(&[]);
        h.store.mode(Mode::Broken);
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap(), None);
        h.clock.fetch_add(60_000, Ordering::SeqCst);
        assert_eq!(h.cache.get_token("jenkins.apiToken").unwrap(), None);
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap(), None);
        let w = h.warnings.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("[credentials] ") && w[0].contains("re-enter your tokens"));
    }

    #[test]
    fn list_names_on_an_undecryptable_store_returns_empty_and_shares_the_latch() {
        let mut h = harness(&[]);
        h.store.mode(Mode::Broken);
        assert!(h.cache.list_names().unwrap().is_empty());
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap(), None);
        assert!(h.cache.list_names().unwrap().is_empty());
        assert_eq!(h.warnings.lock().unwrap().len(), 1, "one warning total, shared");
    }

    #[test]
    fn clearing_a_token_on_an_undecryptable_store_is_a_no_op_not_an_error_and_shares_the_latch() {
        let mut h = harness(&[]);
        h.store.mode(Mode::Broken);
        assert!(h.cache.set_token("jira.apiToken", None).is_ok());
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap(), None);
        assert_eq!(h.warnings.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_successful_set_clears_the_latch_so_a_later_break_warns_again() {
        let mut h = harness(&[]);
        h.store.mode(Mode::Broken);
        h.cache.get_token("jira.apiToken").unwrap();
        assert_eq!(h.warnings.lock().unwrap().len(), 1);
        h.store.mode(Mode::Ok);
        h.cache.set_token("jira.apiToken", Some("fresh-token")).unwrap();
        assert_eq!(h.cache.get_token("jira.apiToken").unwrap().as_deref(), Some("fresh-token"));
        assert_eq!(h.warnings.lock().unwrap().len(), 1, "the store is fine again");
        h.store.mode(Mode::Broken);
        h.cache.get_token("a-name-never-cached-before").unwrap();
        assert_eq!(h.warnings.lock().unwrap().len(), 2, "the latch was reset");
    }

    #[test]
    fn a_swallowed_remove_failure_does_not_reset_the_latch() {
        let mut h = harness(&[]);
        h.store.mode(Mode::Broken);
        h.cache.get_token("jira.apiToken").unwrap();
        h.cache.set_token("jenkins.apiToken", None).unwrap();
        assert!(h.cache.list_names().unwrap().is_empty());
        assert_eq!(h.warnings.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_non_undecryptable_error_is_returned_not_swallowed() {
        let mut h = harness(&[("jira.apiToken", "tok")]);
        h.store.mode(Mode::Failing("disk full".into()));
        let boom = StoreError::Other("disk full".into());
        assert_eq!(h.cache.get_token("jira.apiToken"), Err(boom.clone()));
        assert_eq!(h.cache.list_names(), Err(boom.clone()));
        assert_eq!(h.cache.set_token("jira.apiToken", None), Err(boom.clone()));
        assert_eq!(h.cache.set_token("jira.apiToken", Some("value")), Err(boom));
        assert!(h.warnings.lock().unwrap().is_empty());
    }
}
