//! The active environment profile (port of `environment.js` plus the module
//! singleton behaviour of `require("../environment.js")`).
//!
//! In the Node build a distribution *replaced* `environment.js`, and every
//! module that needed the profile required it. Here the distribution's `main`
//! calls [`init`] once, before anything else, and the rest of the framework
//! reads [`current`]. If nothing was initialised (the framework's own tests, or
//! the bare framework binary) the neutral placeholder profile below is used,
//! whose example.com hosts point nowhere, so a fresh install of the bare
//! framework asks you for your own.

use crate::environment_base::{Environment, define_environment};
use serde_json::{Value, json};
use std::sync::OnceLock;

/// The framework's placeholder profile, as data (what `environment.js` held).
pub fn placeholder_profile() -> Value {
    json!({
        // One entry per external system. `id` is also the config.json key and the credential prefix
        // (`<id>.apiToken`). `kind` says what the system is for; `provider` names the client implementation.
        "sites": [
            // github.com, or your GitHub Enterprise address (setup and Settings can override it). GitHub's browser
            // login doesn't authenticate its API, so the site is token-only and the extension runs only on pull
            // request pages.
            { "id": "github", "kind": "git", "provider": "github", "label": "GitHub", "baseUrl": "https://github.com", "tokenOnly": true, "pageMatches": ["{origin}/*/*/pull/*"] },
            { "id": "jira", "kind": "issues", "provider": "jira-dc", "label": "Jira", "baseUrl": "https://jira.example.com" },
            { "id": "jenkins", "kind": "ci", "provider": "jenkins", "label": "Jenkins", "withUsername": true, "baseUrl": "https://jenkins.example.com" },
        ],
        // Where `companion update` follows. Installs track `ref`.
        "updateSource": { "url": "https://git.example.com/ai-dev-companion.git", "ref": "main" },
        // Defaults for targets: which Jira projects and issue types the analysis features work on.
        "targets": { "projects": ["PROJ"], "issueTypes": [] },
        // How this team uses its issue tracker.
        "issues": { "subtaskParentTypes": ["Story", "Epic"], "subtaskTypeName": "Sub-task", "reviewTransitionName": "In Review" },
        // What the Settings panel calls things; with no `supportEmail` the Support row is hidden. `appSlug` names the install folder, state folder and launchd job (default
        // `ai-dev-companion`; fixed once installed). `productName` is what the product is called in banners,
        // generated comments and the extension's name (default `AI Dev Companion`). `marketplaceName` is the name
        // of the Claude Code plugin marketplace the distribution's repo publishes (default `ai-dev-companion`).
        "branding": {},
        // The Claude Code `--permission-mode` for sessions the features start: "auto" (default), "plan" or
        // "default". `permissionMode` sets it for all of them; `permissionModes` overrides it per feature id,
        // e.g. `{ "review-in-editor": "plan" }` to keep Review PR read-only.
        "claude": {},
        // The pack files whose features this distribution offers.
        "packs": ["./packs/builtin.js"],
    })
}

pub fn placeholder() -> Environment {
    define_environment(&placeholder_profile()).expect("the placeholder profile is valid")
}

static CURRENT: OnceLock<Environment> = OnceLock::new();

/// Installs the distribution's profile. Must be the first thing `main` does:
/// an error if a profile is already in place (including the placeholder, which
/// is installed the first time anything calls [`current`]).
pub fn init(env: Environment) -> Result<(), String> {
    CURRENT.set(env).map_err(|_| "the environment profile was already initialised".to_string())
}

/// Validates `profile` and installs it.
pub fn init_from_profile(profile: &Value) -> Result<(), String> {
    init(define_environment(profile)?)
}

/// The active profile.
pub fn current() -> &'static Environment {
    CURRENT.get_or_init(placeholder)
}

impl Environment {
    /// `branding.<key>` when it is a non-empty string.
    pub fn branding_text(&self, key: &str) -> Option<&str> {
        self.profile.get("branding")?.get(key)?.as_str().filter(|s| !s.is_empty())
    }

    /// A top-level section of the profile (`llm`, `targets`, `issues`, `updateSource`, `claude`, ...).
    pub fn section(&self, name: &str) -> Option<&Value> {
        self.profile.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_site_has_a_unique_id_a_known_kind_and_an_https_base_url() {
        let env = current();
        let ids = env.site_ids();
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        let https_origin = regex::Regex::new(r"^https://[^/]+$").unwrap();
        for site in &env.sites {
            assert!(["git", "issues", "ci"].contains(&site.kind.as_str()), "{}", site.id);
            assert!(https_origin.is_match(&site.base_url), "{}", site.id);
        }
    }

    #[test]
    fn default_base_url_looks_a_site_up_by_id_and_is_empty_for_an_unknown_one() {
        let env = current();
        assert_eq!(env.default_base_url("jira"), env.site_by_id("jira").unwrap().base_url);
        assert_eq!(env.default_base_url("nope"), "");
    }

    #[test]
    fn site_credential_names_follow_the_site_ids() {
        let env = current();
        let want: Vec<String> = env.site_ids().iter().map(|id| format!("{id}.apiToken")).collect();
        assert_eq!(env.site_credential_names(), want);
    }

    #[test]
    fn the_placeholder_matches_what_the_node_profile_held_and_branding_reads_only_real_strings() {
        let env = placeholder();
        assert_eq!(env.site_ids(), ["github", "jira", "jenkins"]);
        assert_eq!(env.packs, ["./packs/builtin.js"]);
        assert_eq!(env.section("updateSource").unwrap()["ref"], "main");
        assert_eq!(env.section("targets").unwrap()["projects"], json!(["PROJ"]));
        assert_eq!(env.branding_text("appSlug"), None);
        let mut p = placeholder_profile();
        p["branding"] = json!({ "appSlug": "acme", "productName": "", "supportEmail": 5 });
        let env = define_environment(&p).unwrap();
        assert_eq!((env.branding_text("appSlug"), env.branding_text("productName"), env.branding_text("supportEmail")), (Some("acme"), None, None));
        assert!(env.site_by_id("github").unwrap().token_only == Some(true));
        assert_eq!(env.site_by_id("jenkins").unwrap().raw["withUsername"], true);
    }

    #[test]
    fn init_is_first_come_and_current_falls_back_to_the_placeholder() {
        // The first reader installs the placeholder, so a later init is refused.
        let _ = current();
        assert!(init(placeholder()).unwrap_err().contains("already initialised"));
        assert!(init_from_profile(&placeholder_profile()).is_err());
        assert!(init_from_profile(&json!({})).is_err(), "an invalid profile is refused");
    }
}
