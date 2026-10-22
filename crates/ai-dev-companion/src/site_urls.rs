//! Setup's "your servers" step (port of `core/site-urls.js`): a profile whose
//! default base URL is a placeholder (the framework's own example.com hosts)
//! can't work as shipped, so setup asks for the real address. A profile with
//! real hosts (every distribution) is never asked, and neither is a site whose
//! address config.json already holds.

use crate::environment_base::{Environment, Site};
use crate::js_text::{js_trim, js_truthy};
use serde_json::{Map, Value, json};
use url::Url;

/// Whether `url` points at an example.com host, i.e. a profile placeholder
/// rather than a real server.
pub fn is_placeholder_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .is_some_and(|h| h == "example.com" || h.ends_with(".example.com"))
}

/// The profile's sites that still need an address: placeholder default and
/// nothing saved in `config`.
pub fn sites_needing_url<'a>(env: &'a Environment, config: &Value) -> Vec<&'a Site> {
    env.sites
        .iter()
        .filter(|s| is_placeholder_url(&s.base_url) && !js_truthy(config.get(&s.id).and_then(|c| c.get("baseUrl"))))
        .collect()
}

/// Normalizes a typed address to an origin-style URL without a trailing slash,
/// or "" when it isn't http(s).
pub fn normalize_url(text: &str) -> String {
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        return String::new();
    }
    let has_scheme = trimmed.find("://").is_some_and(|i| {
        let scheme = &trimmed[..i];
        scheme.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
    });
    let with_scheme = if has_scheme { trimmed.to_string() } else { format!("https://{trimmed}") };
    match Url::parse(&with_scheme) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => with_scheme.trim_end_matches('/').to_string(),
        _ => String::new(),
    }
}

/// What setup's prompt helper offers: `ask(question, default)` returns the
/// typed answer ("" for Enter), `log` prints a line.
pub trait Prompter {
    fn ask(&mut self, question: &str, default: &str) -> String;
    fn log(&mut self, line: &str);
}

/// Asks for the address of each site in [`sites_needing_url`]. Enter skips a
/// site (it stays unconfigured and Settings can set it later). Returns the
/// config fragment to merge: `{ [siteId]: { ...existing, baseUrl } }`.
pub fn prompt_site_urls(env: &Environment, config: &Value, prompter: &mut impl Prompter) -> Map<String, Value> {
    let pending = sites_needing_url(env, config);
    let mut out = Map::new();
    if pending.is_empty() {
        return out;
    }
    prompter.log("This build ships with placeholder server addresses. Enter yours (Enter skips one; \u{2699} Settings can set it later).");
    for site in pending {
        loop {
            let answer = prompter.ask(&format!("{} address, e.g. {}", site.label.as_deref().unwrap_or("undefined"), site.base_url), "");
            if answer.is_empty() {
                break;
            }
            let url = normalize_url(&answer);
            if !url.is_empty() {
                let mut entry = config.get(&site.id).and_then(Value::as_object).cloned().unwrap_or_default();
                entry.insert("baseUrl".into(), json!(url));
                out.insert(site.id.clone(), Value::Object(entry));
                break;
            }
            prompter.log("  That doesn't look like an http(s) address, try again or press Enter to skip.");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment_base::define_environment;

    fn env(sites: Value) -> Environment {
        define_environment(&json!({ "sites": sites, "packs": [] })).unwrap()
    }

    fn three() -> Environment {
        env(json!([
            { "id": "jira", "label": "Jira", "kind": "issues", "provider": "jira", "baseUrl": "https://jira.example.com" },
            { "id": "jenkins", "label": "Jenkins", "kind": "ci", "provider": "jenkins", "baseUrl": "https://jenkins.example.com" },
            { "id": "github", "label": "GitHub", "kind": "git", "provider": "github", "baseUrl": "https://github.com" },
        ]))
    }

    struct Scripted {
        answers: Vec<String>,
        lines: Vec<String>,
        asked: usize,
    }

    impl Prompter for Scripted {
        fn ask(&mut self, _q: &str, _d: &str) -> String {
            self.asked += 1;
            self.answers.remove(0)
        }
        fn log(&mut self, line: &str) {
            self.lines.push(line.into());
        }
    }

    #[test]
    fn only_example_com_hosts_count_as_placeholders() {
        for (url, want) in [
            ("https://jira.example.com", true),
            ("https://example.com", true),
            ("https://EXAMPLE.com", true),
            ("https://github.com", false),
            ("https://notexample.com", false),
            ("garbage", false),
        ] {
            assert_eq!(is_placeholder_url(url), want, "{url}");
        }
    }

    #[test]
    fn a_site_already_configured_or_with_a_real_default_is_not_asked() {
        let e = three();
        let ids = |c: Value| sites_needing_url(&e, &c).iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(json!({})), ["jira", "jenkins"]);
        assert_eq!(ids(json!({ "jira": { "baseUrl": "https://j.acme.io" } })), ["jenkins"]);
        assert_eq!(ids(json!({ "jira": { "baseUrl": "" } })), ["jira", "jenkins"]);
    }

    #[test]
    fn normalize_url_adds_a_scheme_drops_trailing_slashes_and_rejects_non_http() {
        assert_eq!(normalize_url("jira.acme.io/"), "https://jira.acme.io");
        assert_eq!(normalize_url("http://ci.acme.io:8080//"), "http://ci.acme.io:8080");
        assert_eq!(normalize_url("  jira.acme.io  "), "https://jira.acme.io");
        assert_eq!(normalize_url("ftp://x"), "");
        assert_eq!(normalize_url(""), "");
        assert_eq!(normalize_url("   "), "");
        assert_eq!(normalize_url("not a url!!"), "");
    }

    #[test]
    fn prompt_site_urls_keeps_existing_site_fields_re_asks_on_a_bad_answer_and_skips_on_enter() {
        let mut p = Scripted { answers: vec!["not a url!!".into(), "jira.acme.io".into(), "".into()], lines: vec![], asked: 0 };
        let out = prompt_site_urls(&three(), &json!({ "jira": { "username": "me" } }), &mut p);
        assert_eq!(Value::Object(out), json!({ "jira": { "username": "me", "baseUrl": "https://jira.acme.io" } }));
        assert!(p.answers.is_empty());
        assert_eq!(p.asked, 3);
        assert!(p.lines.iter().any(|l| l.contains("doesn't look like an http(s) address")));
    }

    #[test]
    fn prompt_site_urls_asks_nothing_for_a_profile_with_real_hosts() {
        let real = env(json!([{ "id": "github", "label": "GitHub", "kind": "git", "provider": "github", "baseUrl": "https://github.com" }]));
        let mut p = Scripted { answers: vec![], lines: vec![], asked: 0 };
        assert!(prompt_site_urls(&real, &json!({}), &mut p).is_empty());
        assert_eq!((p.asked, p.lines.len()), (0, 0));
    }
}
