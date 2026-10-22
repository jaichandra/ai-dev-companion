//! Fallback tokens another tool on this machine already holds (a pack names
//! where to look), so the companion works without a second copy of the same
//! Jira / Bitbucket tokens (port of `core/external-tokens.js`). Read at call
//! time (cached for a minute), never copied into `credentials.enc`, and used
//! only when the companion has no saved token of its own. A token is handed
//! out only for the host its own config names: the origin a source reports
//! must equal the configured site's, so it can never be sent anywhere else.

use crate::environment;
use crate::js_text::{js_trim, JS_WS_CLASS};
use crate::packs::{ExternalTokenEnv, TokenSource};
use regex::Regex;
use serde_json::{Map, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use url::Url;

const CACHE_MS: f64 = 60_000.0;

/// Where a source reads files from (the real disk, or a fake in tests).
pub trait TokenFs: Send + Sync {
    fn read_text(&self, path: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DiskFs;

impl TokenFs for DiskFs {
    fn read_text(&self, path: &str) -> Option<String> {
        std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

impl ExternalTokenEnv {
    pub fn read_text(&self, path: &str) -> Option<String> {
        self.fs.read_text(path)
    }

    pub fn read_json(&self, path: &str) -> Option<Value> {
        serde_json::from_str(&self.read_text(path)?).ok()
    }
}

/// `KEY=value` lines (`#` comments and anything else ignored), values
/// unquoted when wrapped in matching quotes.
pub fn parse_env_file(text: &str) -> Map<String, Value> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^[{ws}]*([A-Za-z_][A-Za-z0-9_]*)[{ws}]*=[{ws}]*(.*?)[{ws}]*$", ws = JS_WS_CLASS)).unwrap());
    let mut out = Map::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // JS's `.` never matches a line terminator.
        if line.contains(['\r', '\u{2028}', '\u{2029}']) {
            continue;
        }
        let Some(m) = LINE.captures(line) else { continue };
        let raw = &m[2];
        let b = raw.as_bytes();
        let value = if b.len() >= 2 && matches!(b[0], b'"' | b'\'') && b[0] == b[b.len() - 1] { &raw[1..raw.len() - 1] } else { raw };
        out.insert(m[1].to_string(), Value::String(value.to_string()));
    }
    out
}

/// The origin (`https://host[:port]`) of an http(s) URL, or `None`.
pub fn origin_of(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.origin().ascii_serialization())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalToken {
    pub token: String,
    pub username: Option<String>,
    pub source: String,
}

pub struct ExternalTokens {
    home: PathBuf,
    fs: Arc<dyn TokenFs>,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    sources: Vec<TokenSource>,
    cache: Mutex<Option<(Map<String, Value>, f64)>>,
}

impl ExternalTokens {
    pub fn new(home: PathBuf, sources: Vec<TokenSource>) -> Self {
        Self { home, fs: Arc::new(DiskFs), clock: Box::new(crate::job_files::now_ms), sources, cache: Mutex::new(None) }
    }

    pub fn with_fs(mut self, fs: Arc<dyn TokenFs>) -> Self {
        self.fs = fs;
        self
    }

    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    // Each source reads one place a tool keeps a token and returns the entries
    // it found; where to look is the packs' business.
    fn load(&self) -> Map<String, Value> {
        let env = ExternalTokenEnv { home: self.home.clone(), fs: self.fs.clone() };
        let mut found = Map::new();
        for source in &self.sources {
            // One unreadable source never hides the others.
            if let Ok(Value::Object(entries)) = catch_unwind(AssertUnwindSafe(|| source(&env))) {
                found.extend(entries);
            }
        }
        found
    }

    /// The token (plus `username` for Jenkins' HTTP Basic) for `name`
    /// ("jira.apiToken" | "bitbucket.apiToken" | ...) when its origin is `base_url`'s.
    pub fn token_for(&self, name: &str, base_url: &str) -> Option<ExternalToken> {
        let now = (self.clock)();
        let mut cache = self.cache.lock().unwrap();
        if cache.as_ref().is_none_or(|(_, at)| now - at >= CACHE_MS) {
            *cache = Some((self.load(), now));
        }
        let entry = cache.as_ref()?.0.get(name)?;
        let (origin, wanted) = (entry["origin"].as_str().filter(|o| !o.is_empty())?, origin_of(base_url)?);
        if origin != wanted {
            return None;
        }
        Some(ExternalToken {
            token: entry["token"].as_str().map(String::from)?,
            username: entry["username"].as_str().filter(|u| !u.is_empty()).map(String::from),
            source: entry["source"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Which file each site's fallback token would come from, for status
    /// lines. Never the token.
    pub fn sources_for(&self, base_urls: &Map<String, Value>) -> Map<String, Value> {
        let mut out = Map::new();
        for site in environment::current().site_ids() {
            let url = base_urls.get(site).and_then(Value::as_str).filter(|u| !u.is_empty());
            if let Some(hit) = url.and_then(|u| self.token_for(&format!("{site}.apiToken"), u)) {
                out.insert(site.to_string(), Value::String(hit.source));
            }
        }
        out
    }
}

/// Trims like `String.prototype.trim` (for sources that read tokens from files).
pub fn trimmed(text: &str) -> &str {
    js_trim(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    struct MapFs(HashMap<String, String>);
    impl TokenFs for MapFs {
        fn read_text(&self, path: &str) -> Option<String> {
            self.0.get(path).cloned()
        }
    }

    fn fs_with(files: &[(&str, &str)]) -> Arc<dyn TokenFs> {
        Arc::new(MapFs(files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()))
    }

    /// A source like a pack's: a Jira PAT in `~/.tool/.jira.env`, the URL beside it.
    fn jira_env_source() -> TokenSource {
        Arc::new(|env: &ExternalTokenEnv| {
            let text = env.read_text("/h/.tool/.jira.env").unwrap_or_default();
            let vars = parse_env_file(&text);
            match (vars.get("JIRA_PERSONAL_TOKEN").and_then(Value::as_str), vars.get("JIRA_URL").and_then(Value::as_str).and_then(origin_of)) {
                (Some(token), Some(origin)) => json!({ "jira.apiToken": { "token": token, "origin": origin, "source": "~/.tool/.jira.env" } }),
                _ => json!({}),
            }
        })
    }

    fn make(files: &[(&str, &str)], sources: Vec<TokenSource>) -> ExternalTokens {
        ExternalTokens::new(PathBuf::from("/h"), sources).with_fs(fs_with(files))
    }

    const JIRA_ENV: &str = "JIRA_URL=https://jira.example.com\nJIRA_PERSONAL_TOKEN=\"jira-pat\"\n# comment\nOTHER=x\n";

    #[test]
    fn parse_env_file_handles_quotes_comments_and_blank_lines() {
        assert_eq!(Value::Object(parse_env_file("A=1\n\n# c\nB=\"two\"\nC='3'\n")), json!({ "A": "1", "B": "two", "C": "3" }));
        assert_eq!(Value::Object(parse_env_file("  K = v with spaces  \r\nQ=\"unterminated\nR='a'b'\n9X=1\n")), json!({ "K": "v with spaces", "Q": "\"unterminated", "R": "a'b" }));
        assert_eq!(Value::Object(parse_env_file("E=\n")), json!({ "E": "" }));
    }

    #[test]
    fn origin_of_only_accepts_http_and_https() {
        assert_eq!(origin_of("https://jira.example.com/x?y").as_deref(), Some("https://jira.example.com"));
        assert_eq!(origin_of("HTTP://Host:8080/a").as_deref(), Some("http://host:8080"));
        assert_eq!(origin_of("https://h:443/").as_deref(), Some("https://h"));
        for bad in ["ftp://x", "file:///x", "not a url", ""] {
            assert_eq!(origin_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_token_is_handed_out_only_for_the_host_its_config_names() {
        let t = make(&[("/h/.tool/.jira.env", JIRA_ENV)], vec![jira_env_source()]);
        assert_eq!(t.token_for("jira.apiToken", "https://jira.example.com"), Some(ExternalToken { token: "jira-pat".into(), username: None, source: "~/.tool/.jira.env".into() }));
        assert_eq!(t.token_for("jira.apiToken", "https://jira.example.com/browse/X"), t.token_for("jira.apiToken", "https://jira.example.com"));
        assert_eq!(t.token_for("jira.apiToken", "https://elsewhere.example.com"), None, "wrong host gets nothing");
        assert_eq!(t.token_for("jira.apiToken", "http://jira.example.com"), None, "scheme is part of the origin");
        assert_eq!(t.token_for("bitbucket.apiToken", "https://jira.example.com"), None);
        assert_eq!(t.token_for("jira.apiToken", "not a url"), None);
    }

    #[test]
    fn a_token_with_no_url_beside_it_is_never_used_and_missing_files_give_none() {
        let t = make(&[("/h/.tool/.jira.env", "JIRA_PERSONAL_TOKEN=abc\n")], vec![jira_env_source()]);
        assert_eq!(t.token_for("jira.apiToken", "https://jira.example.com"), None);
        assert_eq!(make(&[], vec![jira_env_source()]).token_for("jira.apiToken", "https://jira.example.com"), None);
        assert_eq!(make(&[], vec![]).token_for("jira.apiToken", "https://jira.example.com"), None);
    }

    #[test]
    fn a_source_that_panics_hides_nothing_and_later_sources_win_a_name() {
        let boom: TokenSource = Arc::new(|_| panic!("unreadable"));
        let mine: TokenSource = Arc::new(|_| json!({ "jira.apiToken": { "token": "t1", "origin": "https://jira.example.com", "source": "my file" } }));
        let later: TokenSource = Arc::new(|_| json!({ "jira.apiToken": { "token": "t2", "origin": "https://jira.example.com", "source": "later", "username": "jdoe" } }));
        let junk: TokenSource = Arc::new(|_| json!("not an object"));
        let t = make(&[], vec![boom, mine.clone(), junk]);
        assert_eq!(t.token_for("jira.apiToken", "https://jira.example.com"), Some(ExternalToken { token: "t1".into(), username: None, source: "my file".into() }));
        let t = make(&[], vec![mine, later]);
        assert_eq!(t.token_for("jira.apiToken", "https://jira.example.com"), Some(ExternalToken { token: "t2".into(), username: Some("jdoe".into()), source: "later".into() }));
    }

    #[test]
    fn what_a_source_found_is_cached_for_a_minute() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let source: TokenSource = Arc::new(move |_| {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            json!({ "jira.apiToken": { "token": "t", "origin": "https://jira.example.com", "source": "s" } })
        });
        let now = Arc::new(Mutex::new(0.0));
        let n = now.clone();
        let t = make(&[], vec![source]).with_clock(move || *n.lock().unwrap());
        t.token_for("jira.apiToken", "https://jira.example.com");
        *now.lock().unwrap() = 59_000.0;
        t.token_for("jira.apiToken", "https://jira.example.com");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        *now.lock().unwrap() = 60_000.0;
        t.token_for("jira.apiToken", "https://jira.example.com");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn sources_for_names_the_file_of_each_site_that_has_a_fallback_never_the_token() {
        let t = make(&[("/h/.tool/.jira.env", JIRA_ENV)], vec![jira_env_source()]);
        let urls = json!({ "jira": "https://jira.example.com", "bitbucket": "https://bb.example.com" });
        let out = t.sources_for(urls.as_object().unwrap());
        assert_eq!(Value::Object(out), json!({ "jira": "~/.tool/.jira.env" }));
        assert!(t.sources_for(&Map::new()).is_empty());
    }
}
