//! The extension's `manifest.json`, generated (port of
//! `core/extension-manifest.js`): the hosts it runs on come from the
//! environment profile's sites (with any base URL overridden in config.json),
//! and the content-script list from the packs' features, so a different Jira
//! host or a different set of features never means hand-editing the manifest.
//!
//! Only the host lists, the script list and the name (the profile's product
//! name) are generated: the version, permissions and background worker are read
//! back from the existing file, so a version bump stays a one-place edit.

use crate::app_identity::product_name;
use crate::environment_base::{Environment, Site};
use crate::external_tokens::origin_of;
use crate::packs::PackRegistry;
use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::Path;

/// Scripts every install loads, in order, before any feature file: they share
/// one global scope, and the registry must exist before the features register.
pub const BASE_SCRIPTS: [&str; 10] = ["icons.js", "diff-viewer.js", "open-in-editor.js", "settings-panel.js", "target-patterns.js", "jenkins-urls.js", "registry.js", "ui-kit.js", "jira-helpers.js", "git-host.js"];
/// Loaded last: it reads the registry once at startup.
pub const FINAL_SCRIPT: &str = "content.js";
/// The service the extension talks to; any port.
pub const LOCAL_HOST: &str = "http://127.0.0.1/*";

fn configured_base_url<'a>(config: &'a Value, site: &'a Site) -> &'a str {
    config[&site.id]["baseUrl"].as_str().filter(|b| !b.is_empty()).unwrap_or(&site.base_url)
}

/// The origin of every site (`None` when its URL isn't http(s)), with
/// config.json's base URL overriding the profile's.
pub fn site_origins(env: &Environment, config: &Value) -> Vec<(String, Option<String>)> {
    env.sites.iter().map(|s| (s.id.clone(), origin_of(configured_base_url(config, s)))).collect()
}

fn origin_for(origins: &[(String, Option<String>)], id: &str) -> Option<String> {
    origins.iter().find(|(i, _)| i == id).and_then(|(_, o)| o.clone())
}

/// The origin of the site of each kind ("git" | "issues" | "ci"), for
/// `GET /targets`: the extension skips a feature on a page of any other origin.
pub fn origins_by_kind(env: &Environment, config: &Value) -> Map<String, Value> {
    let origins = site_origins(env, config);
    let mut out = Map::new();
    for site in &env.sites {
        if !out.contains_key(&site.kind) {
            if let Some(o) = origin_for(&origins, &site.id) {
                out.insert(site.kind.clone(), json!(o));
            }
        }
    }
    out
}

/// Where the button sits on each site that sets a `fabAnchor`
/// (`[{origin, right?, bottom?}]`), for `GET /targets`: a profile uses it to
/// clear a site's own floating buttons.
pub fn fab_anchors(env: &Environment, config: &Value) -> Vec<Value> {
    let origins = site_origins(env, config);
    env.sites
        .iter()
        .filter_map(|s| {
            let anchor = s.fab_anchor.as_ref()?;
            let mut m = Map::new();
            m.insert("origin".into(), json!(origin_for(&origins, &s.id)?));
            for (k, v) in anchor {
                m.insert(k.clone(), crate::js_number::json_number(*v));
            }
            Some(Value::Object(m))
        })
        .collect()
}

/// The kind-by-kind provider ids of the profile (`{git: "github", issues:
/// "jira-dc", ci: "jenkins"}`), for `GET /targets`: the extension reads pull
/// request pages the way that git host lays them out.
pub fn providers_by_kind(env: &Environment) -> Map<String, Value> {
    let mut out = Map::new();
    for site in &env.sites {
        if !out.contains_key(&site.kind) {
            out.insert(site.kind.clone(), json!(site.provider));
        }
    }
    out
}

fn dedupe(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// Host permissions (`https://host/*`), de-duplicated, in profile order. A
/// token-only site (GitHub) is left out: its browser login is no use to the
/// companion, so the extension is given no reason to read its cookies.
pub fn host_patterns(env: &Environment, config: &Value) -> Vec<String> {
    let origins = site_origins(env, config);
    dedupe(env.sites.iter().filter(|s| s.token_only != Some(true)).filter_map(|s| origin_for(&origins, &s.id)).map(|o| format!("{o}/*")))
}

/// The match patterns of a site's pages for the extension: the whole origin,
/// or the profile's `pageMatches` (e.g. only pull request pages on GitHub)
/// with `{origin}` replaced by the effective origin.
fn page_patterns(site: &Site, origin: Option<&str>) -> Vec<String> {
    let Some(origin) = origin else { return vec![] };
    match &site.page_matches {
        Some(p) => p.iter().map(|x| x.replacen("{origin}", origin, 1)).collect(),
        None => vec![format!("{origin}/*")],
    }
}

/// Where the content scripts run: every site's pages (narrowed by `pageMatches`), de-duplicated.
pub fn match_patterns(env: &Environment, config: &Value) -> Vec<String> {
    let origins = site_origins(env, config);
    dedupe(env.sites.iter().flat_map(|s| page_patterns(s, origin_for(&origins, &s.id).as_deref())))
}

/// Match patterns of the sites whose pages show stack traces (git hosts and
/// CI), for the "Open in editor" right-click menu; the issue tracker is left
/// out on purpose.
pub fn context_menu_matches(env: &Environment, config: &Value) -> Vec<String> {
    let origins = site_origins(env, config);
    dedupe(env.sites.iter().filter(|s| s.kind == "git" || s.kind == "ci").flat_map(|s| page_patterns(s, origin_for(&origins, &s.id).as_deref())))
}

/// Feature scripts (and the packs' own extra ones), in the menu's order: `extension.order`, ties by pack order.
pub fn feature_scripts<F>(packs: &PackRegistry<F>) -> Vec<String> {
    let features = packs.features().filter_map(|s| s.extension.as_ref());
    let mut specs: Vec<(usize, f64, &str)> = features.chain(packs.extension_scripts()).enumerate().filter(|(_, e)| !e.script.is_empty()).map(|(i, e)| (i, e.order.unwrap_or(1000.0), e.script.as_str())).collect();
    specs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    specs.into_iter().map(|(_, _, s)| s.to_string()).collect()
}

/// Every script of the content-script list, in load order.
pub fn content_scripts<F>(packs: &PackRegistry<F>) -> Vec<String> {
    BASE_SCRIPTS.iter().map(|s| s.to_string()).chain(feature_scripts(packs)).chain([FINAL_SCRIPT.to_string()]).collect()
}

/// `base` (the existing manifest) with the generated fields filled in.
pub fn build_manifest<F>(base: &Value, config: &Value, env: &Environment, packs: &PackRegistry<F>) -> Value {
    let mut manifest = base.as_object().cloned().unwrap_or_default();
    manifest.insert("name".into(), json!(product_name()));
    let mut hosts = host_patterns(env, config);
    hosts.push(LOCAL_HOST.to_string());
    manifest.insert("host_permissions".into(), json!(hosts));
    let mut scripts: Vec<Value> = base["content_scripts"].as_array().cloned().unwrap_or_default();
    let mut first = if scripts.is_empty() { Map::new() } else { scripts.remove(0).as_object().cloned().unwrap_or_default() };
    first.insert("matches".into(), json!(match_patterns(env, config)));
    first.insert("js".into(), json!(content_scripts(packs)));
    scripts.insert(0, Value::Object(first));
    for page in packs.page_scripts() {
        scripts.retain(|b| b["js"] != json!(page.js));
        scripts.push(json!({ "matches": page.matches, "js": page.js, "run_at": "document_idle" }));
    }
    manifest.insert("content_scripts".into(), Value::Array(scripts));
    Value::Object(manifest)
}

pub fn render(manifest: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(manifest).expect("a JSON value serializes"))
}

/// The generated file `background.js` loads via `importScripts`: kept out of
/// `background.js` itself so updating the extension's code can never reset the
/// secret to a placeholder, and the secret never lands in git.
pub fn render_extension_config(secret: &str, port: u16, context_menu_matches: &[String]) -> String {
    let value = json!({ "serviceBaseUrl": format!("http://127.0.0.1:{port}"), "sharedSecret": secret, "contextMenuMatches": context_menu_matches });
    format!(
        "// Generated by companion-service/setup.js \u{2014} do not edit or commit.\n// Must match companion-service/config.json; re-run `npm run setup` to regenerate.\nself.COMPANION_CONFIG = {};\n",
        serde_json::to_string_pretty(&value).expect("a JSON value serializes")
    )
}

/// Whether the hosts the extension must cover differ between two configs.
pub fn hosts_changed(env: &Environment, before: &Value, after: &Value) -> bool {
    let shape = |c: &Value| (host_patterns(env, c), match_patterns(env, c));
    shape(before) != shape(after)
}

/// Writes `text` owner-only (0600), also tightening a file an earlier version left readable by
/// others: companion-config.js holds the shared secret.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

/// Writes what the extension folder needs from `config`: `companion-config.js`
/// (service address, secret, right-click menu hosts) and a manifest whose hosts
/// and scripts match. Returns whether the manifest changed.
pub fn write_extension_files<F>(extension_dir: &Path, config: &Value, env: &Environment, packs: &PackRegistry<F>) -> io::Result<bool> {
    let port = config["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
    write_private(&extension_dir.join("companion-config.js"), &render_extension_config(config["sharedSecret"].as_str().unwrap_or(""), port, &context_menu_matches(env, config)))?;
    write_manifest(extension_dir, config, env, packs)
}

/// Rewrites `<extension_dir>/manifest.json` when its generated fields differ.
/// Returns true if it changed.
pub fn write_manifest<F>(extension_dir: &Path, config: &Value, env: &Environment, packs: &PackRegistry<F>) -> io::Result<bool> {
    let file = extension_dir.join("manifest.json");
    let current = fs::read_to_string(&file)?;
    let parsed: Value = serde_json::from_str(&current).map_err(io::Error::other)?;
    let next = render(&build_manifest(&parsed, config, env, packs));
    if next == current {
        return Ok(false);
    }
    fs::write(file, next)?;
    Ok(true)
}

/// Problems between the manifest on disk and what config + packs call for (empty when it matches).
pub fn check_manifest<F>(extension_dir: &Path, config: &Value, env: &Environment, packs: &PackRegistry<F>) -> Vec<String> {
    let file = extension_dir.join("manifest.json");
    let current: Value = match fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
        Ok(c) => c,
        Err(e) => return vec![format!("{} can't be read ({e}).", file.display())],
    };
    let want = build_manifest(&current, config, env, packs);
    let mut problems = vec![];
    if current["host_permissions"] != want["host_permissions"] {
        problems.push(format!("host_permissions should be {}.", want["host_permissions"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
    }
    let have = &current["content_scripts"][0];
    let want_first = &want["content_scripts"][0];
    if have["matches"] != want_first["matches"] {
        problems.push(format!("content_scripts.matches should be {}.", want_first["matches"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
    }
    if have["js"] != want_first["js"] {
        problems.push("content_scripts.js doesn't list the features of the loaded packs.".into());
    }
    for script in want_first["js"].as_array().unwrap().iter().filter_map(Value::as_str) {
        if !extension_dir.join(script).exists() {
            problems.push(format!("{script} is missing from the extension folder."));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment;
    use crate::packs::{ExtensionSpec, FeatureDescriptor, FeatureSpec, Pack};
    use std::path::PathBuf;

    fn extension_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("chrome-extension")
    }

    fn packs() -> PackRegistry<()> {
        let spec = |id: &str, order: Option<f64>| {
            let mut s = FeatureSpec::new(FeatureDescriptor::new(id, id), ());
            s.extension = Some(ExtensionSpec { script: format!("features/{id}.js"), order });
            s
        };
        let none = FeatureSpec::new(FeatureDescriptor::new("no-ui", "no-ui"), ());
        PackRegistry::new(vec![Pack::new("t", vec![spec("resolve-conflict", Some(20.0)), spec("review-in-editor", Some(10.0)), none, spec("analyze-issue", None), spec("create-jira-subtasks", Some(20.0))])]).unwrap()
    }

    #[test]
    fn a_packs_page_scripts_become_content_script_blocks_once() {
        let mut pack = Pack::<()>::new("t", vec![]);
        pack.page_scripts = vec![crate::packs::PageScript { matches: vec!["http://*.localhost/*".into()], js: vec!["packs/t/beat.js".into()] }];
        let registry = PackRegistry::new(vec![pack]).unwrap();
        let base = json!({ "content_scripts": [{ "matches": ["https://x/*"], "js": ["a.js"] }] });
        let once = build_manifest(&base, &json!({}), environment::current(), &registry);
        assert_eq!(once["content_scripts"][1], json!({ "matches": ["http://*.localhost/*"], "js": ["packs/t/beat.js"], "run_at": "document_idle" }));
        let twice = build_manifest(&once, &json!({}), environment::current(), &registry);
        assert_eq!(twice["content_scripts"].as_array().unwrap().len(), 2);
    }

    fn committed() -> Value {
        json!({
            "manifest_version": 3,
            "name": "Old",
            "version": "1.2.3",
            "permissions": ["storage", "cookies"],
            "host_permissions": ["https://old.example.com/*", "http://127.0.0.1/*"],
            "background": { "service_worker": "background.js" },
            "content_scripts": [{ "matches": ["https://old.example.com/*"], "js": ["x.js"], "run_at": "document_idle" }, { "matches": ["https://other/*"], "js": ["y.js"] }],
        })
    }

    #[test]
    fn a_base_url_saved_in_config_replaces_the_profiles_host_and_a_shared_host_is_listed_once() {
        let env = environment::current();
        let config = json!({ "jira": { "baseUrl": "https://tracker.mycompany.test/some/path" }, "jenkins": { "baseUrl": "https://tracker.mycompany.test" } });
        let hosts = host_patterns(env, &config);
        assert_eq!(hosts, ["https://tracker.mycompany.test/*"], "github is token-only, so it has no host permission; the shared host appears once");
        let built = build_manifest(&committed(), &config, env, &packs());
        assert_eq!(built["host_permissions"], json!(["https://tracker.mycompany.test/*", "http://127.0.0.1/*"]));
    }

    #[test]
    fn the_profiles_defaults_give_each_site_its_host() {
        let env = environment::current();
        assert_eq!(host_patterns(env, &json!({})), ["https://jira.example.com/*", "https://jenkins.example.com/*"]);
        assert_eq!(match_patterns(env, &json!({})), ["https://github.com/*/*/pull/*", "https://jira.example.com/*", "https://jenkins.example.com/*"], "GitHub only on pull request pages");
    }

    #[test]
    fn a_base_url_that_isnt_http_falls_out_instead_of_reaching_the_manifest() {
        let hosts = host_patterns(environment::current(), &json!({ "jira": { "baseUrl": "ftp://nope" }, "jenkins": { "baseUrl": "not a url" } }));
        assert!(hosts.is_empty());
    }

    #[test]
    fn only_the_generated_fields_change_the_name_comes_from_the_profile_version_permissions_and_the_worker_are_kept() {
        let mut base = committed();
        base["version"] = json!("9.9.9");
        let built = build_manifest(&base, &json!({ "jira": { "baseUrl": "https://x.example.com" } }), environment::current(), &packs());
        assert_eq!(built["version"], "9.9.9");
        assert_eq!(built["name"], product_name());
        assert_eq!(built["permissions"], base["permissions"]);
        assert_eq!(built["background"], base["background"]);
        assert_eq!(built["content_scripts"][0]["run_at"], "document_idle");
        assert_eq!(built["content_scripts"][1], base["content_scripts"][1], "later content script blocks are kept");
        assert_eq!(built.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(), ["manifest_version", "name", "version", "permissions", "host_permissions", "background", "content_scripts"], "the key order of the existing file is kept");
    }

    #[test]
    fn feature_scripts_follow_the_menu_order_ties_by_pack_order() {
        assert_eq!(feature_scripts(&packs()), ["features/review-in-editor.js", "features/resolve-conflict.js", "features/create-jira-subtasks.js", "features/analyze-issue.js"]);
        let all = content_scripts(&packs());
        assert_eq!(all.first().map(String::as_str), Some("icons.js"));
        assert_eq!(all.last().map(String::as_str), Some("content.js"));
        assert_eq!(all.len(), BASE_SCRIPTS.len() + 4 + 1);
    }

    #[test]
    fn the_right_click_menu_is_for_the_git_host_and_ci_server_not_the_issue_tracker() {
        let matches = context_menu_matches(environment::current(), &json!({}));
        assert_eq!(matches, ["https://github.com/*/*/pull/*", "https://jenkins.example.com/*"]);
    }

    #[test]
    fn origins_by_kind_gives_the_origin_of_the_git_issues_and_ci_site_honouring_config() {
        let env = environment::current();
        let o = origins_by_kind(env, &json!({}));
        assert_eq!(o.keys().cloned().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>(), ["ci", "git", "issues"]);
        assert_eq!(origins_by_kind(env, &json!({ "github": { "baseUrl": "https://git.example.com/x" } }))["git"], "https://git.example.com");
        assert_eq!(providers_by_kind(env), json!({ "git": "github", "issues": "jira-dc", "ci": "jenkins" }).as_object().unwrap().clone());
    }

    #[test]
    fn fab_anchors_lists_the_origin_and_offsets_of_each_site_that_sets_one() {
        let mut env = environment::current().clone();
        assert!(fab_anchors(&env, &json!({})).is_empty());
        env.sites[1].fab_anchor = Some(vec![("bottom".into(), 96.0)]);
        assert_eq!(fab_anchors(&env, &json!({})), [json!({ "origin": "https://jira.example.com", "bottom": 96 })]);
    }

    #[test]
    fn check_manifest_names_what_is_wrong_with_a_stale_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let env = environment::current();
        fs::write(dir.path().join("manifest.json"), serde_json::to_string(&committed()).unwrap()).unwrap();
        let problems = check_manifest(dir.path(), &json!({}), env, &packs());
        assert!(problems.iter().any(|p| p.starts_with("host_permissions should be")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("is missing from the extension folder")));
        assert!(problems.iter().any(|p| p.contains("content_scripts.js doesn't list")));
        assert!(write_manifest(dir.path(), &json!({}), env, &packs()).unwrap());
        assert!(!write_manifest(dir.path(), &json!({}), env, &packs()).unwrap(), "a second write changes nothing");
        assert!(check_manifest(dir.path(), &json!({}), env, &packs()).iter().all(|p| p.contains("is missing from the extension folder")), "hosts and scripts now match");
        assert!(check_manifest(&dir.path().join("nope"), &json!({}), env, &packs())[0].contains("can't be read"));
    }

    #[test]
    fn every_base_script_and_final_script_exists_in_the_extension_folder() {
        for script in BASE_SCRIPTS.iter().chain([&FINAL_SCRIPT]) {
            assert!(extension_dir().join(script).exists(), "{script} is listed but missing");
        }
    }

    #[test]
    fn render_extension_config_carries_the_service_address_secret_and_right_click_hosts() {
        let text = render_extension_config("s3cret", 4321, &["https://ci.example.com/*".to_string()]);
        let body = text.lines().skip(2).collect::<Vec<_>>().join("\n");
        let json_text = body.trim().strip_prefix("self.COMPANION_CONFIG = ").unwrap().trim_end_matches(';');
        assert_eq!(serde_json::from_str::<Value>(json_text).unwrap(), json!({ "serviceBaseUrl": "http://127.0.0.1:4321", "sharedSecret": "s3cret", "contextMenuMatches": ["https://ci.example.com/*"] }));
        assert!(text.starts_with("// Generated by companion-service/setup.js"));
    }

    #[test]
    fn hosts_changed_is_true_only_when_the_hosts_the_extension_must_cover_differ() {
        let env = environment::current();
        let a = |u: &str| json!({ "jira": { "baseUrl": u } });
        assert!(!hosts_changed(env, &json!({}), &json!({})));
        assert!(!hosts_changed(env, &a("https://a.example.com"), &a("https://a.example.com/other/path")), "same origin");
        assert!(hosts_changed(env, &json!({}), &a("https://a.example.com")));
        assert!(hosts_changed(env, &a("https://a.example.com"), &json!({})));
    }

    #[test]
    fn the_connection_file_is_owner_only_even_over_a_world_readable_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("companion-config.js");
        fs::write(&file, "old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&file, "new").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn write_extension_files_writes_the_connection_file_and_brings_the_manifest_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let env = environment::current();
        fs::write(dir.path().join("manifest.json"), serde_json::to_string(&committed()).unwrap()).unwrap();
        let config = json!({ "sharedSecret": "abc", "port": 8123, "jira": { "baseUrl": "https://jira.example.com" } });
        assert!(write_extension_files(dir.path(), &config, env, &packs()).unwrap());
        let text = fs::read_to_string(dir.path().join("companion-config.js")).unwrap();
        assert!(text.contains("\"serviceBaseUrl\": \"http://127.0.0.1:8123\"") && text.contains("\"sharedSecret\": \"abc\""));
        let manifest: Value = serde_json::from_str(&fs::read_to_string(dir.path().join("manifest.json")).unwrap()).unwrap();
        assert!(manifest["host_permissions"].as_array().unwrap().contains(&json!("https://jira.example.com/*")));
        assert!(!write_extension_files(dir.path(), &config, env, &packs()).unwrap(), "nothing to change the second time");
    }
}
