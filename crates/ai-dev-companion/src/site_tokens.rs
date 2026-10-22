//! Where a site's API token comes from: the companion's own saved one first,
//! then the one another tool on this machine already stored for the same host
//! (`external_tokens`). Shared by the Bitbucket, GitHub, Jira and Jenkins clients.

use crate::credentials::TokenLookup;
use crate::external_tokens::ExternalTokens;
use std::sync::Arc;

pub struct SiteTokens {
    pub credentials: Arc<dyn TokenLookup>,
    pub external: ExternalTokens,
}

/// A token and, for HTTP Basic sites, the username it goes with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub token: String,
    pub username: Option<String>,
}

impl SiteTokens {
    pub fn new(credentials: Arc<dyn TokenLookup>, external: ExternalTokens) -> Self {
        Self { credentials, external }
    }

    /// The saved token for `name` (`bitbucket.apiToken`, ...), else the borrowed one for `base_url`'s host.
    pub fn resolve(&self, name: &str, base_url: &str) -> Option<Token> {
        if let Some(t) = self.credentials.get_token(name) {
            return Some(Token { token: t, username: None });
        }
        self.external.token_for(name, base_url).map(|e| Token { token: e.token, username: e.username })
    }

    pub fn token(&self, name: &str, base_url: &str) -> Option<String> {
        self.resolve(name, base_url).map(|t| t.token)
    }
}

/// Trailing slashes off, and a scheme added if the configured value is missing
/// one: the setup wizard and the Settings panel take the base URL as free text.
pub fn normalize_base_url(raw: &str) -> String {
    let trimmed = crate::js_text::js_trim(raw).trim_end_matches('/').to_string();
    let lower = trimmed.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") { trimmed } else { format!("https://{trimmed}") }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::packs::TokenSource;

    pub struct Fixed(pub Vec<(&'static str, &'static str)>);
    impl TokenLookup for Fixed {
        fn get_token(&self, name: &str) -> Option<String> {
            self.0.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
        }
    }

    pub fn tokens(saved: Vec<(&'static str, &'static str)>) -> SiteTokens {
        SiteTokens::new(Arc::new(Fixed(saved)), ExternalTokens::new(std::path::PathBuf::from("/h"), Vec::<TokenSource>::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn the_saved_token_wins_over_a_borrowed_one_and_a_borrowed_one_needs_the_matching_host() {
        let source: crate::packs::TokenSource = Arc::new(|_| json!({ "jira.apiToken": { "token": "borrowed", "origin": "https://jira.example.com", "source": "x", "username": "jdoe" } }));
        let t = SiteTokens::new(Arc::new(Fixed(vec![("bitbucket.apiToken", "own")])), ExternalTokens::new("/h".into(), vec![source]));
        assert_eq!(t.token("bitbucket.apiToken", "https://bb.example.com").as_deref(), Some("own"));
        assert_eq!(t.resolve("jira.apiToken", "https://jira.example.com"), Some(Token { token: "borrowed".into(), username: Some("jdoe".into()) }));
        assert_eq!(t.resolve("jira.apiToken", "https://elsewhere.example.com"), None);
        assert_eq!(tokens(vec![]).token("jira.apiToken", "https://jira.example.com"), None);
    }

    #[test]
    fn base_urls_lose_trailing_slashes_and_gain_a_scheme() {
        assert_eq!(normalize_base_url(" https://bb.example.com/// "), "https://bb.example.com");
        assert_eq!(normalize_base_url("bb.example.com/"), "https://bb.example.com");
        assert_eq!(normalize_base_url("HTTP://x"), "HTTP://x");
    }
}
