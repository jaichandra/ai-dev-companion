//! `config.json`: loading, the soft-migration of the feature list, repo mappings
//! and the atomic write (port of `config.ts`). The config is a plain JSON value
//! (every reader takes `&Value`, so a Settings change applies on the next call);
//! this store owns the file and the in-memory copy.
//!
//! Unlike the Node service there is no `dist/` copy to go stale: the store is
//! given the one real path.

use crate::feature_registry::FeatureRegistry;
use crate::prereqs::{default_search_roots, infer_repo_path};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The live config, read afresh by every provider and scheduler call.
pub type ConfigFn = std::sync::Arc<dyn Fn() -> Value + Send + Sync>;

pub const DEFAULT_SUMMARIZE_COMMENTS_MODEL: &str = "claude-haiku-4-5-20251001";
const MAX_COMPONENT_REPOS: usize = 50;

/// config.json is missing or unreadable: nothing can run until setup has been, so the service
/// exits cleanly instead of crash-looping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigMissingError(pub String);

impl std::fmt::Display for ConfigMissingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigMissingError {}

/// No local clone of project/repo could be found. The service turns this into a 409 the extension
/// answers by offering to pick a folder or clone it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoNotFoundError {
    pub project: String,
    pub repo: String,
    pub message: String,
}

impl std::fmt::Display for RepoNotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RepoNotFoundError {}

pub struct ConfigStore {
    path: PathBuf,
    value: Mutex<Value>,
}

fn lock(m: &Mutex<Value>) -> MutexGuard<'_, Value> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn as_object(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().expect("just made an object")
}

impl ConfigStore {
    /// Reads and normalizes config.json: `repos` always exists, and the enabled-features list is
    /// soft-migrated when new descriptors ship (persisted, so the next start doesn't re-log it).
    pub fn load(path: &Path, features: Option<&FeatureRegistry>) -> Result<Self, ConfigMissingError> {
        let mut config = read_file(path).map_err(|reason| {
            let dir = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            ConfigMissingError(format!("{} {reason}. Run the setup wizard first: cd {dir} && companion setup", path.display()))
        })?;
        match config["sharedSecret"].as_str() {
            Some(s) if !s.is_empty() && !s.starts_with("REPLACE_WITH") => {}
            _ => eprintln!("[config] sharedSecret is not set in config.json \u{2014} every request will be rejected until you set it (and set the same value in chrome-extension/background.js)."),
        }
        // `repos` is only ever written by a feature's own setup, so a config written while no such
        // feature was enabled has no key at all. Normalized once, so every reader can trust it.
        if !config["repos"].is_object() {
            as_object(&mut config).insert("repos".into(), json!({}));
        }
        let store = Self { path: path.to_path_buf(), value: Mutex::new(config.clone()) };
        if let Some(migration) = features.and_then(|f| f.migrate_enabled_features(&config)) {
            let was: Vec<&str> = config["enabledFeatures"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let added: Vec<&String> = migration.enabled_features.iter().filter(|id| !was.contains(&id.as_str())).collect();
            {
                let mut v = lock(&store.value);
                let o = as_object(&mut v);
                o.insert("enabledFeatures".into(), json!(migration.enabled_features));
                o.insert("knownFeatures".into(), json!(migration.known_features));
            }
            store.write().map_err(|e| ConfigMissingError(format!("{} can't be written ({e})", path.display())))?;
            if !added.is_empty() {
                println!("[config] Enabled new feature(s) added since your last setup: {}. Re-run `companion setup` if you want to turn any of them off.", added.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The config as it is now (read afresh by every provider call).
    pub fn snapshot(&self) -> Value {
        lock(&self.value).clone()
    }

    /// Replaces the in-memory config (a Settings save) and writes the file.
    pub fn replace(&self, config: Value) -> std::io::Result<()> {
        *lock(&self.value) = config;
        self.write()
    }

    /// Resolves the local clone path for a "PROJECT/repo" key. If it isn't mapped yet, tries to
    /// infer it from where your *other* configured repos live (`infer_repo_path` only accepts a
    /// candidate once its own git origin actually matches this exact project/repo). A successful
    /// inference is persisted so it doesn't need to be re-discovered. Fails only if neither an
    /// explicit mapping nor a confident inference is available. A mapping whose folder is no
    /// longer a git checkout (moved or deleted) is dropped and resolved afresh.
    pub fn repo_path(&self, project: &str, repo: &str, home: &Path) -> Result<String, RepoNotFoundError> {
        let key = format!("{project}/{repo}");
        let existing = {
            let mut v = lock(&self.value);
            let repos = as_object(&mut v).entry("repos").or_insert_with(|| json!({}));
            let repos = as_object(repos);
            let found = repos.keys().find(|k| k.to_lowercase() == key.to_lowercase()).cloned();
            match found {
                Some(k) => {
                    let mapped = repos[&k].as_str().unwrap_or_default().to_string();
                    if Path::new(&mapped).join(".git").exists() {
                        return Ok(mapped);
                    }
                    eprintln!("[config] repos[\"{k}\"] = \"{mapped}\" is no longer a git checkout; looking again.");
                    repos.shift_remove(&k);
                    repos.values().filter_map(|p| p.as_str().map(PathBuf::from)).collect::<Vec<_>>()
                }
                None => repos.values().filter_map(|p| p.as_str().map(PathBuf::from)).collect(),
            }
        };
        let Some(inferred) = infer_repo_path(&existing, project, repo, &default_search_roots(home)) else {
            return Err(RepoNotFoundError {
                project: project.into(),
                repo: repo.into(),
                message: format!("No local clone of \"{key}\" was found (looked next to your other clones and in common folders like ~/gitviews, ~/git, ~/src). Choose its folder or let the assistant clone it from the \u{2728} menu, or clone it yourself next to your other repos and try again."),
            });
        };
        let inferred = inferred.to_string_lossy().into_owned();
        let _ = self.save_repo(&key, &inferred);
        println!("[config] Found repos[\"{key}\"] = \"{inferred}\" and saved it to config.json.");
        Ok(inferred)
    }

    /// Adds (or replaces) a repo mapping and writes it, so it survives a restart. The repos map is
    /// a cache the service fills itself: by inference, a folder the user picked, or a clone it made.
    pub fn save_repo(&self, key: &str, clone_path: &str) -> std::io::Result<()> {
        {
            let mut v = lock(&self.value);
            let repos = as_object(&mut v).entry("repos").or_insert_with(|| json!({}));
            as_object(repos).insert(key.into(), json!(clone_path));
        }
        self.write()
    }

    /// Remembers that tickets with this Jira component belong in this repo, so the next one is
    /// matched without asking Claude. Never replaces an entry that is already there; the map stays
    /// small. Returns whether it saved.
    pub fn save_component_repo(&self, component: &str, repo_key: &str) -> std::io::Result<bool> {
        {
            let mut v = lock(&self.value);
            let analyze = as_object(&mut v).entry("analyzeIssue").or_insert_with(|| json!({}));
            let map = as_object(as_object(analyze).entry("componentRepoMap").or_insert_with(|| json!({})));
            if map.len() >= MAX_COMPONENT_REPOS || map.keys().any(|k| k.to_lowercase() == component.to_lowercase()) {
                return Ok(false);
            }
            map.insert(component.into(), json!(repo_key));
        }
        self.write()?;
        Ok(true)
    }

    /// Writes the current config: see [`write_config`].
    pub fn write(&self) -> std::io::Result<()> {
        write_config(&self.path, &lock(&self.value))
    }
}

/// config.json exactly as saved (no normalizing or migrating). The error is the reason, worded for
/// "`<path>` <reason>".
fn read_file(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "doesn't exist yet".to_string() } else { format!("can't be read ({e})") })?;
    serde_json::from_str(&text).map_err(|e| format!("can't be read ({e})"))
}

/// config.json exactly as saved: no normalizing or migrating, unlike [`ConfigStore::load`].
pub fn read_config_file(path: &Path) -> Result<Value, String> {
    read_file(path).map_err(|r| format!("{} {r}", path.display()))
}

/// Replaces config.json via a temp file + rename so a crash never leaves a half-written file.
/// Keeps the existing file's permissions (it holds the shared secret and any API tokens); a new
/// file is created owner-only.
pub fn write_config(path: &Path, config: &Value) -> std::io::Result<()> {
    let mode = fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), crate::job_files::now_ms() as i64));
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        // `mode` is subject to the umask at creation; restore the exact permissions.
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all((serde_json::to_string_pretty(config).expect("a JSON value serializes") + "\n").as_bytes())?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The default base URL of a profile site (`jira`, `jenkins`, `bitbucket`).
pub fn default_base_url(id: &str) -> String {
    crate::environment::current().default_base_url(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_cli::testing::git as sh;

    fn write(dir: &Path, v: Value) -> PathBuf {
        let p = dir.join("config.json");
        fs::write(&p, v.to_string()).unwrap();
        p
    }

    #[test]
    fn a_missing_or_broken_file_is_a_setup_message_and_a_missing_repos_key_is_filled_in() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        let e = ConfigStore::load(&p, None).err().unwrap();
        assert_eq!(e.0, format!("{} doesn't exist yet. Run the setup wizard first: cd {} && companion setup", p.display(), d.path().display()));
        fs::write(&p, "{nope").unwrap();
        assert!(ConfigStore::load(&p, None).err().unwrap().0.contains("can't be read ("));
        write(d.path(), json!({ "port": 1, "sharedSecret": "s" }));
        let s = ConfigStore::load(&p, None).unwrap();
        assert_eq!(s.snapshot(), json!({ "port": 1, "sharedSecret": "s", "repos": {} }));
        assert_eq!(read_config_file(&p).unwrap(), json!({ "port": 1, "sharedSecret": "s" }), "the file itself isn't rewritten by a plain load");
    }

    #[test]
    fn new_feature_ids_are_auto_enabled_and_the_migration_is_persisted() {
        let d = tempfile::tempdir().unwrap();
        let desc = |id: &str, on: Option<bool>| {
            let mut f = crate::packs::FeatureDescriptor::new(id, id);
            f.enabled_by_default = on;
            f
        };
        let registry = FeatureRegistry::new(vec![desc("a", None), desc("b", None), desc("c", Some(false))]);
        let p = write(d.path(), json!({ "sharedSecret": "s", "enabledFeatures": ["a"], "knownFeatures": ["a"] }));
        let s = ConfigStore::load(&p, Some(&registry)).unwrap();
        let on_disk = read_config_file(&p).unwrap();
        assert_eq!(on_disk["enabledFeatures"], json!(["a", "b"]));
        assert_eq!(on_disk["knownFeatures"], json!(["a", "b", "c"]));
        assert_eq!(s.snapshot()["enabledFeatures"], on_disk["enabledFeatures"]);
        // Nothing left to migrate the second time: no rewrite.
        let before = fs::metadata(&p).unwrap().modified().unwrap();
        ConfigStore::load(&p, Some(&registry)).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn writes_are_atomic_keep_the_files_permissions_and_a_new_file_is_owner_only() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        write_config(&p, &json!({ "a": 1 })).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\n  \"a\": 1\n}\n");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        write_config(&p, &json!({ "a": 2 })).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp file is left behind");
        // A directory that can't be written to fails cleanly.
        assert!(write_config(&d.path().join("missing/config.json"), &json!({})).is_err());
    }

    #[test]
    fn repo_mappings_are_saved_replaced_and_survive_a_reload() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), json!({ "sharedSecret": "s" }));
        let s = ConfigStore::load(&p, None).unwrap();
        s.save_repo("P/r", "/x/r").unwrap();
        s.save_repo("P/r", "/y/r").unwrap();
        assert_eq!(ConfigStore::load(&p, None).unwrap().snapshot()["repos"], json!({ "P/r": "/y/r" }));
    }

    #[test]
    fn component_repos_are_remembered_once_case_insensitively_and_capped() {
        let d = tempfile::tempdir().unwrap();
        let s = ConfigStore::load(&write(d.path(), json!({ "sharedSecret": "s" })), None).unwrap();
        assert!(s.save_component_repo("Billing", "P/billing").unwrap());
        assert!(!s.save_component_repo("billing", "P/other").unwrap(), "never replaces an entry");
        assert_eq!(s.snapshot()["analyzeIssue"]["componentRepoMap"], json!({ "Billing": "P/billing" }));
        for i in 0..60 {
            let _ = s.save_component_repo(&format!("c{i}"), "P/x");
        }
        assert_eq!(s.snapshot()["analyzeIssue"]["componentRepoMap"].as_object().unwrap().len(), 50);
        assert!(!s.save_component_repo("one-more", "P/x").unwrap());
    }

    fn clone_of(dir: &Path, name: &str, origin: &str) -> PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(&p).unwrap();
        sh(&p, &["init", "-q"]);
        sh(&p, &["remote", "add", "origin", origin]);
        p
    }

    #[test]
    fn a_mapped_repo_is_used_a_stale_mapping_is_dropped_and_an_unmapped_one_is_inferred_next_to_the_others_and_saved() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let git_dir = d.path().join("clones");
        fs::create_dir_all(&home).unwrap();
        let known = clone_of(&git_dir, "known", "https://bb.example.com/scm/ACME/known.git");
        let wanted = clone_of(&git_dir, "wanted", "https://bb.example.com/scm/ACME/wanted.git");
        let p = write(d.path(), json!({ "sharedSecret": "s", "repos": { "ACME/known": known, "ACME/gone": "/no/such/clone" } }));
        let s = ConfigStore::load(&p, None).unwrap();
        assert_eq!(s.repo_path("acme", "KNOWN", &home).unwrap(), known.to_string_lossy());
        // Not mapped: found next to the known clone, origin confirmed, and persisted.
        assert_eq!(s.repo_path("ACME", "wanted", &home).unwrap(), wanted.to_string_lossy());
        assert_eq!(read_config_file(&p).unwrap()["repos"]["ACME/wanted"], json!(wanted));
        // A stale mapping is dropped and, with no clone anywhere, reported as not found.
        let e = s.repo_path("ACME", "gone", &home).unwrap_err();
        assert_eq!((e.project.as_str(), e.repo.as_str()), ("ACME", "gone"));
        assert!(e.message.starts_with("No local clone of \"ACME/gone\" was found"));
        assert!(s.snapshot()["repos"].get("ACME/gone").is_none());
    }

    #[test]
    fn replace_swaps_the_whole_config_and_writes_it() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), json!({ "sharedSecret": "s" }));
        let s = ConfigStore::load(&p, None).unwrap();
        s.replace(json!({ "sharedSecret": "t", "repos": {} })).unwrap();
        assert_eq!(read_config_file(&p).unwrap()["sharedSecret"], "t");
        assert_eq!(s.path(), p);
        assert_eq!(default_base_url("jira"), crate::environment::current().default_base_url("jira"));
        assert_eq!(DEFAULT_SUMMARIZE_COMMENTS_MODEL, "claude-haiku-4-5-20251001");
    }
}
