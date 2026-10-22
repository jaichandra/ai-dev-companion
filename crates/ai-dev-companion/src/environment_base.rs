//! Builds an environment profile from plain data (port of
//! `core/environment-base.js`). A profile is everything that says "this is one
//! team's setup" rather than "this is how the tool works": the servers it
//! talks to, where updates come from, which Jira projects to default to, which packs to load, what to call things. A distribution
//! supplies one as JSON data; the framework ships a neutral placeholder.
//!
//! Code elsewhere reads these values through the helpers below and never
//! hardcodes a host, a job name or a model. Layers, lowest to highest
//! precedence: the profile, then config.json; the profile only supplies
//! defaults, nothing in it is a hard limit.

use crate::js_text::js_stringify;
use serde_json::{Map, Value};
use url::Url;

pub const SITE_KINDS: [&str; 3] = ["git", "issues", "ci"];
/// The `--permission-mode` values a Claude Code session can be started (and
/// resumed) with.
pub const PERMISSION_MODES: [&str; 3] = ["plan", "auto", "default"];

#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    pub id: String,
    pub kind: String,
    pub provider: String,
    pub base_url: String,
    pub label: Option<String>,
    pub token_only: Option<bool>,
    /// Pixels from the page's right and bottom edges.
    pub fab_anchor: Option<Vec<(String, f64)>>,
    pub page_matches: Option<Vec<String>>,
    /// Everything else the profile says about the site, kept as given.
    pub raw: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Environment {
    pub sites: Vec<Site>,
    pub packs: Vec<String>,
    /// The whole profile as given (a distribution's own sections included).
    pub profile: Map<String, Value>,
}

impl Environment {
    /// Every site id, in profile order.
    pub fn site_ids(&self) -> Vec<&str> {
        self.sites.iter().map(|s| s.id.as_str()).collect()
    }

    pub fn site_by_id(&self, id: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.id == id)
    }

    /// The default base URL of site `id` ("" when the profile has no such site).
    pub fn default_base_url(&self, id: &str) -> &str {
        self.site_by_id(id).map_or("", |s| s.base_url.as_str())
    }

    /// The credential names that belong to sites: `jira.apiToken`, ...
    pub fn site_credential_names(&self) -> Vec<String> {
        self.sites.iter().map(|s| format!("{}.apiToken", s.id)).collect()
    }

    /// Credential names a distribution's own settings sections save (the profile's `credentialNames`).
    pub fn extra_credential_names(&self) -> Vec<String> {
        self.profile.get("credentialNames").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
    }
}

fn fail<T>(message: impl AsRef<str>) -> Result<T, String> {
    Err(format!("Invalid environment profile: {}", message.as_ref()))
}

fn check_mode(what: &str, v: &Value) -> Result<(), String> {
    match v.as_str() {
        Some(m) if PERMISSION_MODES.contains(&m) => Ok(()),
        _ => fail(format!(
            "claude.{what} must be one of {}, got {}",
            PERMISSION_MODES.join(", "),
            js_stringify(Some(v))
        )),
    }
}

/// Checks a profile's optional `claude` section: `permissionMode`, and
/// `permissionModes` keyed by feature id.
pub fn validate_claude_section(claude: Option<&Value>) -> Result<(), String> {
    let Some(claude) = claude else { return Ok(()) };
    let obj = match claude {
        Value::Object(o) => o,
        Value::Array(_) => return Ok(()), // an array is a JS object with no such fields
        _ => return fail("claude must be an object"),
    };
    if let Some(mode) = obj.get("permissionMode") {
        check_mode("permissionMode", mode)?;
    }
    match obj.get("permissionModes") {
        Some(Value::Object(modes)) => {
            for (id, mode) in modes {
                check_mode(&format!("permissionModes.{id}"), mode)?;
            }
        }
        None | Some(Value::Null) | Some(Value::Bool(false)) | Some(Value::Array(_)) => {}
        Some(_) => return fail("claude.permissionModes must be an object"),
    }
    Ok(())
}

fn is_site_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty() && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn parse_site(site: &Value, seen: &mut Vec<String>) -> Result<Site, String> {
    let obj = site.as_object();
    let id = obj.and_then(|o| o.get("id")).and_then(Value::as_str);
    let Some(id) = id.filter(|id| is_site_id(id)) else {
        // `site && site.id`: a falsy site shows itself, anything else its id.
        let shown = match site {
            Value::Null | Value::Bool(false) => site.to_string(),
            Value::String(s) if s.is_empty() => site.to_string(),
            Value::Number(n) if n.as_f64() == Some(0.0) => site.to_string(),
            _ => js_stringify(obj.and_then(|o| o.get("id"))),
        };
        return fail(format!("a site needs a lowercase id, got {shown}"));
    };
    let obj = obj.expect("a site with an id is an object");
    if seen.iter().any(|s| s == id) {
        return fail(format!("site \"{id}\" is listed twice"));
    }
    seen.push(id.to_string());
    let kind = obj.get("kind").and_then(Value::as_str).filter(|k| SITE_KINDS.contains(k));
    let Some(kind) = kind else {
        return fail(format!(
            "site \"{id}\" has kind {}, expected one of {}",
            js_stringify(obj.get("kind")),
            SITE_KINDS.join(", ")
        ));
    };
    let provider = obj.get("provider").and_then(Value::as_str).filter(|p| !p.is_empty());
    let Some(provider) = provider else { return fail(format!("site \"{id}\" needs a provider")) };
    let base_url = obj.get("baseUrl").and_then(Value::as_str).filter(|u| Url::parse(u).is_ok());
    let Some(base_url) = base_url else { return fail(format!("site \"{id}\" needs a baseUrl that is a URL")) };
    let token_only = match obj.get("tokenOnly") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return fail(format!("site \"{id}\": tokenOnly must be true or false")),
    };
    let fab_anchor = match obj.get("fabAnchor") {
        None => None,
        Some(a) => {
            let parsed = a.as_object().filter(|m| !m.is_empty()).and_then(|m| {
                m.iter()
                    .map(|(k, v)| {
                        let n = v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)?;
                        (k == "right" || k == "bottom").then(|| (k.clone(), n))
                    })
                    .collect::<Option<Vec<_>>>()
            });
            match parsed {
                Some(p) => Some(p),
                None => {
                    return fail(format!(
                        "site \"{id}\": fabAnchor must be {{ right?, bottom? }} in pixels from the page's right and bottom edges, e.g. {{ bottom: 96 }}"
                    ));
                }
            }
        }
    };
    let page_matches = match obj.get("pageMatches") {
        None => None,
        Some(m) => {
            let parsed = m.as_array().filter(|a| !a.is_empty()).and_then(|a| {
                a.iter().map(|x| x.as_str().filter(|s| s.starts_with("{origin}/")).map(str::to_string)).collect::<Option<Vec<_>>>()
            });
            match parsed {
                Some(p) => Some(p),
                None => {
                    return fail(format!(
                        "site \"{id}\": pageMatches must be a list of patterns starting with \"{{origin}}/\", e.g. \"{{origin}}/*/*/pull/*\""
                    ));
                }
            }
        }
    };
    Ok(Site {
        id: id.into(),
        kind: kind.into(),
        provider: provider.into(),
        base_url: base_url.into(),
        label: obj.get("label").and_then(Value::as_str).map(str::to_string),
        token_only,
        fab_anchor,
        page_matches,
        raw: obj.clone(),
    })
}

/// Checks `profile` and returns it with the lookup helpers.
pub fn define_environment(profile: &Value) -> Result<Environment, String> {
    let Some(map) = profile.as_object() else { return fail("expected an object") };
    validate_claude_section(map.get("claude"))?;
    let site_values = match map.get("sites") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return fail("sites must be a non-empty list"),
    };
    let mut seen = Vec::new();
    let sites = site_values.iter().map(|s| parse_site(s, &mut seen)).collect::<Result<Vec<_>, _>>()?;
    let packs = match map.get("packs") {
        Some(Value::Array(a)) => a.iter().map(|p| p.as_str().filter(|s| !s.is_empty()).map(str::to_string)).collect::<Option<Vec<_>>>(),
        _ => None,
    };
    let Some(packs) = packs else { return fail("packs must be a list of pack files") };
    Ok(Environment { sites, packs, profile: map.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn site(id: &str) -> Value {
        json!({ "id": id, "kind": "git", "provider": "bitbucket", "baseUrl": "https://bb.example.com", "label": "Bitbucket" })
    }

    fn profile(extra: Value) -> Value {
        let mut p = json!({ "sites": [site("bitbucket"), { "id": "jira", "kind": "issues", "provider": "jira", "baseUrl": "https://jira.example.com" }], "packs": ["packs/builtin.js"], "branding": { "name": "X" } });
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    }

    fn err(p: Value) -> String {
        define_environment(&p).unwrap_err()
    }

    #[test]
    fn a_valid_profile_gets_the_lookup_helpers_and_keeps_its_other_sections() {
        let env = define_environment(&profile(json!({}))).unwrap();
        assert_eq!(env.site_ids(), ["bitbucket", "jira"]);
        assert_eq!(env.site_by_id("jira").unwrap().kind, "issues");
        assert!(env.site_by_id("nope").is_none());
        assert_eq!(env.default_base_url("bitbucket"), "https://bb.example.com");
        assert_eq!(env.default_base_url("nope"), "");
        assert_eq!(env.site_credential_names(), ["bitbucket.apiToken", "jira.apiToken"]);
        assert_eq!(env.profile["branding"], json!({ "name": "X" }));
        assert_eq!(env.site_by_id("bitbucket").unwrap().label.as_deref(), Some("Bitbucket"));
    }

    #[test]
    fn rejects_a_malformed_profile_with_a_message_that_says_what_is_wrong() {
        assert_eq!(err(json!(null)), "Invalid environment profile: expected an object");
        assert_eq!(err(profile(json!({ "sites": [] }))), "Invalid environment profile: sites must be a non-empty list");
        assert_eq!(err(profile(json!({ "sites": "x" }))), "Invalid environment profile: sites must be a non-empty list");
        assert!(err(profile(json!({ "packs": "x" }))).ends_with("packs must be a list of pack files"));
        assert!(err(profile(json!({ "packs": [""] }))).ends_with("packs must be a list of pack files"));
        assert!(err(profile(json!({ "packs": [1] }))).ends_with("packs must be a list of pack files"));
        assert!(err(profile(json!({ "sites": [site("a"), site("a")] }))).ends_with("site \"a\" is listed twice"));
    }

    #[test]
    fn rejects_bad_site_fields() {
        let with = |k: &str, v: Value| {
            let mut s = site("bitbucket");
            s[k] = v;
            profile(json!({ "sites": [s] }))
        };
        assert!(err(with("id", json!("Bad"))).ends_with("a site needs a lowercase id, got \"Bad\""));
        assert!(err(profile(json!({ "sites": [null] }))).ends_with("a site needs a lowercase id, got null"));
        assert!(err(profile(json!({ "sites": [{}] }))).ends_with("a site needs a lowercase id, got undefined"));
        assert!(err(with("kind", json!("vcs"))).ends_with("site \"bitbucket\" has kind \"vcs\", expected one of git, issues, ci"));
        assert!(err(with("provider", json!(""))).ends_with("site \"bitbucket\" needs a provider"));
        assert!(err(with("baseUrl", json!("not a url"))).ends_with("site \"bitbucket\" needs a baseUrl that is a URL"));
        assert!(err(with("tokenOnly", json!("yes"))).ends_with("tokenOnly must be true or false"));
        assert!(err(with("fabAnchor", json!({ "top": 1 }))).contains("fabAnchor must be { right?, bottom? }"));
        assert!(err(with("fabAnchor", json!({}))).contains("fabAnchor must be"));
        assert!(err(with("fabAnchor", json!({ "right": -1 }))).contains("fabAnchor must be"));
        assert!(err(with("pageMatches", json!(["/x"]))).contains("pageMatches must be a list"));
        assert!(err(with("pageMatches", json!([]))).contains("pageMatches must be a list"));
    }

    #[test]
    fn accepts_the_optional_site_fields_when_valid() {
        let mut s = site("bitbucket");
        s["tokenOnly"] = json!(true);
        s["fabAnchor"] = json!({ "bottom": 96, "right": 12 });
        s["pageMatches"] = json!(["{origin}/*/*/pull/*"]);
        let env = define_environment(&profile(json!({ "sites": [s] }))).unwrap();
        let s = &env.sites[0];
        assert_eq!(s.token_only, Some(true));
        assert_eq!(s.fab_anchor, Some(vec![("bottom".into(), 96.0), ("right".into(), 12.0)]));
        assert_eq!(s.page_matches, Some(vec!["{origin}/*/*/pull/*".to_string()]));
    }

    #[test]
    fn the_claude_section_is_checked() {
        assert!(define_environment(&profile(json!({ "claude": { "permissionMode": "plan", "permissionModes": { "digest": "auto" } } }))).is_ok());
        assert_eq!(
            err(profile(json!({ "claude": { "permissionMode": "bypass" } }))),
            "Invalid environment profile: claude.permissionMode must be one of plan, auto, default, got \"bypass\""
        );
        assert!(err(profile(json!({ "claude": { "permissionModes": { "digest": 3 } } })))
            .ends_with("claude.permissionModes.digest must be one of plan, auto, default, got 3"));
        assert!(err(profile(json!({ "claude": "x" }))).ends_with("claude must be an object"));
        assert!(err(profile(json!({ "claude": null }))).ends_with("claude must be an object"));
        // The claude section is checked before the sites.
        assert!(err(json!({ "claude": "x", "sites": [] })).ends_with("claude must be an object"));
    }
}
