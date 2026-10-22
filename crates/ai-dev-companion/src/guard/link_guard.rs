//! Links that came from a service (a PR or ticket URL) end up in the inbox and
//! are opened on a click. Only https links on the configured Bitbucket or Jira
//! host are kept; anything else becomes `None` (port of `core/link-guard.js`).

use serde_json::Value;
use url::Url;

/// `host[:port]` as JS `URL.host` reports it: lowercase, default port omitted.
fn js_host(u: &Url) -> Option<String> {
    let host = u.host_str()?.to_lowercase();
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// `url` normalized when it is https, has no credentials and its host is one
/// of `base_urls`' hosts; else `None`. An unusable configured base URL allows
/// nothing.
pub fn link_on_hosts<S: AsRef<str>>(url: &str, base_urls: &[S]) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let u = Url::parse(url).ok()?;
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()) {
        return None;
    }
    let host = js_host(&u)?;
    base_urls
        .iter()
        .filter_map(|b| Url::parse(b.as_ref()).ok().as_ref().and_then(js_host))
        .any(|b| b == host)
        .then(|| u.to_string())
}

/// The same over an untyped value: anything but a string is `None`.
pub fn link_on_hosts_value<S: AsRef<str>>(url: Option<&Value>, base_urls: &[S]) -> Option<String> {
    link_on_hosts(url?.as_str()?, base_urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASES: [&str; 2] = ["https://bb.example", "https://jira.example:8443/"];

    #[test]
    fn only_https_links_on_a_configured_host_survive() {
        let ok = |u: &str| link_on_hosts(u, &BASES);
        assert_eq!(ok("https://bb.example/projects/CI").as_deref(), Some("https://bb.example/projects/CI"));
        assert_eq!(ok("https://BB.example/x").as_deref(), Some("https://bb.example/x"));
        assert_eq!(
            ok("https://jira.example:8443/browse/PROJ-1").as_deref(),
            Some("https://jira.example:8443/browse/PROJ-1")
        );
        for bad in [
            "http://bb.example/x",
            "https://evil.example/x",
            "https://jira.example/x", // wrong port
            "https://bb.example.evil.example/x",
            "https://user:pw@bb.example/x",
            "javascript:alert(1)",
            "//bb.example/x",
            "not a url",
            "",
        ] {
            assert_eq!(ok(bad), None, "{bad}");
        }
        for bad in [json!(null), json!(42)] {
            assert_eq!(link_on_hosts_value(Some(&bad), &BASES), None, "{bad}");
        }
        assert_eq!(link_on_hosts_value(None, &BASES), None);
        assert_eq!(link_on_hosts("https://bb.example/x", &[] as &[&str]), None, "no configured host allows nothing");
        assert_eq!(link_on_hosts("https://bb.example/x", &["nonsense"]), None);
    }

    // Beyond the ported test: the default port is omitted on both sides, as
    // with JS `URL.host`.
    #[test]
    fn the_default_https_port_is_the_same_host() {
        assert!(link_on_hosts("https://bb.example:443/x", &["https://bb.example"]).is_some());
        assert!(link_on_hosts("https://bb.example/x", &["https://bb.example:443/"]).is_some());
        assert!(link_on_hosts("https://user@bb.example/x", &["https://bb.example"]).is_none());
        assert!(link_on_hosts("https://:pw@bb.example/x", &["https://bb.example"]).is_none());
    }
}
