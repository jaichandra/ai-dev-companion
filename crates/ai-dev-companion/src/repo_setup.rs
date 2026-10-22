//! Mapping "PROJECT/repo" to a local clone from the extension (port of
//! `core/repo-setup.ts`): pick an existing folder with the native macOS chooser,
//! or clone the repo next to the user's other clones as a job the panel polls.

use crate::config::ConfigStore;
use crate::jobs::JobStore;
use crate::prereqs::{check_repo_path, clone_root, default_search_roots, derive_clone_url, get_origin_url, is_safe_repo_segment_value, origin_matches_project_repo, GitRemoteRules};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// A user-facing problem with the request (bad folder, cancelled picker, unsupported platform):
/// the service maps it to a 400 rather than a 500.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSetupError {
    pub message: String,
    pub code: Option<&'static str>,
}

impl RepoSetupError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: None }
    }
    fn coded(message: impl Into<String>, code: &'static str) -> Self {
        Self { message: message.into(), code: Some(code) }
    }
}

impl std::fmt::Display for RepoSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RepoSetupError {}

pub struct RepoRequest {
    pub project: String,
    pub repo: String,
    pub key: String,
}

pub fn assert_repo_request(body: &Value) -> Result<RepoRequest, RepoSetupError> {
    if !is_safe_repo_segment_value(body.get("project")) || !is_safe_repo_segment_value(body.get("repo")) {
        return Err(RepoSetupError::new("Expected a body with a valid project and repo."));
    }
    let (project, repo) = (body["project"].as_str().unwrap_or_default().to_string(), body["repo"].as_str().unwrap_or_default().to_string());
    Ok(RepoRequest { key: format!("{project}/{repo}"), project, repo })
}

fn escape_apple_script(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What the service uses to ask for a folder and to clone: the commands, injectable for tests.
#[derive(Debug, Clone)]
pub struct RepoSetupTools {
    pub osascript: String,
    pub git: String,
    pub require_macos: bool,
}

impl Default for RepoSetupTools {
    fn default() -> Self {
        Self { osascript: "osascript".into(), git: "git".into(), require_macos: true }
    }
}

pub struct RepoSetup {
    pub config: Arc<ConfigStore>,
    pub jobs: Arc<JobStore>,
    pub tools: RepoSetupTools,
    pub rules: GitRemoteRules,
    pub home: PathBuf,
    /// Lower-cased "PROJECT/repo" -> id of its running clone job.
    clones_in_progress: Arc<Mutex<HashMap<String, String>>>,
}

fn repo_paths(config: &Value) -> Vec<PathBuf> {
    config["repos"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(PathBuf::from)).collect()).unwrap_or_default()
}

impl RepoSetup {
    pub fn new(config: Arc<ConfigStore>, jobs: Arc<JobStore>, rules: GitRemoteRules, home: &Path) -> Self {
        Self { config, jobs, tools: RepoSetupTools::default(), rules, home: home.to_path_buf(), clones_in_progress: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn clone_root(&self, config: &Value) -> PathBuf {
        clone_root(&repo_paths(config), &default_search_roots(&self.home), &self.home)
    }

    /// Native macOS folder picker, brought to the front (the service runs in the background, so
    /// without `activate` the dialog can open behind Chrome). `None` if the user cancels.
    async fn pick_folder(&self, prompt: &str, default_dir: &Path) -> Option<String> {
        let location = if default_dir.exists() { format!(" default location (POSIX file \"{}\")", escape_apple_script(&default_dir.to_string_lossy())) } else { String::new() };
        let script = format!("tell current application\n  activate\n  POSIX path of (choose folder with prompt \"{}\"{location})\nend tell", escape_apple_script(prompt));
        let out = Command::new(&self.tools.osascript).args(["-e", &script]).output().await.ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().trim_end_matches('/').to_string())
    }

    /// Lets the user point at an existing clone, verifies it really is project/repo, and saves
    /// the mapping. Returns the saved path.
    pub async fn choose_repo_folder(&self, body: &Value) -> Result<String, RepoSetupError> {
        let RepoRequest { project, repo, key } = assert_repo_request(body)?;
        if self.tools.require_macos && !cfg!(target_os = "macos") {
            return Err(RepoSetupError::coded(format!("Picking a folder from the browser needs macOS. Add \"{key}\": \"<path to its clone>\" to repos in config.json instead, or let the assistant clone it."), "unsupported"));
        }
        let config = self.config.snapshot();
        let picked = self.pick_folder(&format!("Select your local clone of {key}"), &self.clone_root(&config)).await.filter(|p| !p.is_empty());
        let Some(picked) = picked else { return Err(RepoSetupError::coded("No folder was selected.", "cancelled")) };

        let check = check_repo_path(&picked);
        if !check.ok {
            return Err(RepoSetupError::new(check.message));
        }
        let origin = get_origin_url(&picked);
        if !origin_matches_project_repo(origin.as_deref(), &project, &repo) {
            return Err(RepoSetupError::new(format!("{picked} isn't a clone of {key} \u{2014} its origin is {}.", origin.as_deref().unwrap_or("null"))));
        }
        self.config.save_repo(&key, &picked).map_err(|e| RepoSetupError::new(e.to_string()))?;
        println!("[repos] {key} -> {picked} (chosen in the browser)");
        Ok(picked)
    }

    /// Starts cloning project/repo next to the user's other clones and returns the job to poll.
    /// The clone URL is derived server-side (never taken from the request) and git is told never
    /// to prompt, since there's no terminal to answer on; a credentials problem fails with
    /// instructions instead of hanging. Must be called inside the tokio runtime.
    pub fn start_clone(&self, body: &Value, page_origin: Option<&str>) -> Result<Value, RepoSetupError> {
        let RepoRequest { project, repo, key } = assert_repo_request(body)?;
        let lower = key.to_lowercase();
        // A half-written clone already has a matching origin, so a second request mid-clone must
        // join the running job, not "reuse" the folder below.
        let running = self.clones_in_progress.lock().unwrap().get(&lower).cloned();
        if let Some(id) = running {
            return Ok(self.jobs.get(&id).unwrap_or_else(|| self.jobs.create("clone-repo", Map::from_iter([("key".to_string(), json!(key))]))));
        }
        let config = self.config.snapshot();
        let origins: Vec<String> = repo_paths(&config).iter().filter_map(|p| get_origin_url(&p.to_string_lossy())).collect();
        let Some(url) = derive_clone_url(&origins, page_origin, &project, &repo, self.rules) else {
            return Err(RepoSetupError::new(format!("Couldn't work out a clone URL for {key} \u{2014} reload the Bitbucket page and try again.")));
        };
        let target = self.clone_root(&config).join(&repo);
        let target_str = target.to_string_lossy().into_owned();
        if target.exists() {
            if origin_matches_project_repo(get_origin_url(&target_str).as_deref(), &project, &repo) {
                self.config.save_repo(&key, &target_str).map_err(|e| RepoSetupError::new(e.to_string()))?;
                let job = self.jobs.create("clone-repo", Map::from_iter([("key".to_string(), json!(key)), ("target".to_string(), json!(target_str))]));
                let id = job["id"].as_str().unwrap_or_default();
                return self
                    .jobs
                    .update(id, Map::from_iter([("status".to_string(), json!("approved")), ("result".to_string(), json!({ "summary": format!("Using {target_str}."), "files": [] }))]))
                    .map_err(RepoSetupError::new);
            }
            return Err(RepoSetupError::new(format!("{target_str} already exists and isn't a clone of {key}. Choose its folder instead.")));
        }

        let job = self.jobs.create("clone-repo", Map::from_iter([("key".to_string(), json!(key)), ("target".to_string(), json!(target_str)), ("url".to_string(), json!(url))]));
        let id = job["id"].as_str().unwrap_or_default().to_string();
        let progress = |label: String| {
            let _ = self.jobs.set(&id, "progress", json!({ "stepId": "clone", "label": label }));
        };
        progress(format!("Cloning {key} into {target_str}\u{2026}"));
        self.clones_in_progress.lock().unwrap().insert(lower.clone(), id.clone());
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut cmd = Command::new(&self.tools.git);
        cmd.args(["clone", "--progress", &url, &target_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", std::env::var("GIT_SSH_COMMAND").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "ssh -o BatchMode=yes".into()));
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.clones_in_progress.lock().unwrap().remove(&lower);
                let _ = self.jobs.update(&id, Map::from_iter([("status".to_string(), json!("failed")), ("error".to_string(), json!(format!("Couldn't run git: {e}")))]));
                return Ok(self.jobs.get(&id).unwrap_or(job));
            }
        };
        let (jobs, config_store, in_progress) = (self.jobs.clone(), self.config.clone(), self.clones_in_progress.clone());
        let (job_id, target2, url2) = (id.clone(), target_str.clone(), url.clone());
        tokio::spawn(async move {
            let mut child = child;
            let mut stderr = child.stderr.take().expect("piped");
            let mut tail = String::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                tail.push_str(&text);
                tail = crate::js_text::slice_utf16_end(&tail, 4000);
                let latest = text.split(['\r', '\n']).rfind(|l| has_percent(l));
                if let Some(latest) = latest {
                    let _ = jobs.set(&job_id, "progress", json!({ "stepId": "clone", "label": format!("Cloning {key}: {}", latest.trim()) }));
                }
            }
            let code = child.wait().await.ok().and_then(|s| s.code());
            in_progress.lock().unwrap().remove(&lower);
            if code == Some(0) {
                let _ = config_store.save_repo(&key, &target2);
                println!("[repos] {key} -> {target2} (cloned from {url2})");
                let _ = jobs.update(&job_id, Map::from_iter([("status".to_string(), json!("approved")), ("result".to_string(), json!({ "summary": format!("Cloned {key} into {target2}."), "files": [] }))]));
                return;
            }
            let _ = std::fs::remove_dir_all(&target2);
            let detail: Vec<&str> = tail.split(['\r', '\n']).filter(|l| !l.trim().is_empty() && !has_percent(l)).collect();
            let detail = detail[detail.len().saturating_sub(5)..].join("\n");
            let _ = jobs.update(
                &job_id,
                Map::from_iter([
                    ("status".to_string(), json!("failed")),
                    ("error".to_string(), json!(format!("Cloning {key} failed:\n{detail}\n\nIf git needs a password or passphrase, clone it once in a terminal instead \u{2014} it will be found automatically afterwards:\n  git clone {url2} {target2}"))),
                ]),
            );
        });
        Ok(job)
    }
}

/// Whether the line contains `\d+%` (a git progress percentage).
fn has_percent(line: &str) -> bool {
    line.as_bytes().windows(2).any(|w| w[0].is_ascii_digit() && w[1] == b'%')
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git_cli::testing::{git as sh, make_repos};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    struct Fx {
        _d: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        setup: RepoSetup,
    }

    fn script(path: &Path, body: &str) -> String {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn fx(existing: &[(&str, &Path)]) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let repos: Map<String, Value> = existing.iter().map(|(k, p)| (k.to_string(), json!(p))).collect();
        let cfg = root.join("config.json");
        fs::write(&cfg, json!({ "sharedSecret": "s", "repos": repos }).to_string()).unwrap();
        let setup = RepoSetup::new(Arc::new(ConfigStore::load(&cfg, None).unwrap()), Arc::new(JobStore::new()), GitRemoteRules::BitbucketDc, &home);
        Fx { _d: d, root, home, setup }
    }

    fn clone_of(dir: &Path, name: &str, origin: &str) -> PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(&p).unwrap();
        sh(&p, &["init", "-q"]);
        sh(&p, &["remote", "add", "origin", origin]);
        p
    }

    async fn settled(jobs: &JobStore, id: &str) -> Value {
        for _ in 0..400 {
            let j = jobs.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the clone never finished");
    }

    #[test]
    fn the_request_needs_a_safe_project_and_repo() {
        assert_eq!(assert_repo_request(&json!({ "project": "ACME", "repo": "app" })).unwrap().key, "ACME/app");
        for bad in [json!(null), json!({}), json!({ "project": "ACME" }), json!({ "project": "../x", "repo": "app" }), json!({ "project": "ACME", "repo": ".git" }), json!({ "project": 1, "repo": "app" })] {
            assert_eq!(assert_repo_request(&bad).err().unwrap().message, "Expected a body with a valid project and repo.", "{bad}");
        }
        assert!(has_percent("Receiving objects:  45% (1/2)") && !has_percent("Cloning into 'x'..."));
    }

    #[tokio::test]
    async fn a_picked_folder_is_verified_against_project_repo_and_saved() {
        let mut f = fx(&[]);
        let good = clone_of(&f.root.join("clones"), "app", "https://bb.example.com/scm/ACME/app.git");
        f.setup.tools = RepoSetupTools { osascript: script(&f.root.join("osa"), &format!("echo '{}/'", good.display())), require_macos: false, ..Default::default() };
        let body = json!({ "project": "ACME", "repo": "app" });
        assert_eq!(f.setup.choose_repo_folder(&body).await.unwrap(), good.to_string_lossy());
        assert_eq!(f.setup.config.snapshot()["repos"]["ACME/app"], json!(good));
        // A folder that is some other repo is refused, naming its origin.
        let e = f.setup.choose_repo_folder(&json!({ "project": "ACME", "repo": "other" })).await.unwrap_err();
        assert_eq!(e.message, format!("{} isn't a clone of ACME/other \u{2014} its origin is https://bb.example.com/scm/ACME/app.git.", good.display()));
        // Not a git repository at all.
        let plain = f.root.join("plain");
        fs::create_dir(&plain).unwrap();
        f.setup.tools.osascript = script(&f.root.join("osa2"), &format!("echo '{}'", plain.display()));
        assert!(f.setup.choose_repo_folder(&body).await.unwrap_err().message.contains("is not a git repository"));
    }

    #[tokio::test]
    async fn cancelling_the_picker_and_a_non_macos_host_have_their_own_codes() {
        let mut f = fx(&[]);
        f.setup.tools = RepoSetupTools { osascript: script(&f.root.join("osa"), "exit 1"), require_macos: false, ..Default::default() };
        let e = f.setup.choose_repo_folder(&json!({ "project": "ACME", "repo": "app" })).await.unwrap_err();
        assert_eq!((e.message.as_str(), e.code), ("No folder was selected.", Some("cancelled")));
        f.setup.tools.osascript = "/no/osascript".into();
        assert_eq!(f.setup.choose_repo_folder(&json!({ "project": "ACME", "repo": "app" })).await.unwrap_err().code, Some("cancelled"));
        if !cfg!(target_os = "macos") {
            f.setup.tools.require_macos = true;
            let e = f.setup.choose_repo_folder(&json!({ "project": "ACME", "repo": "app" })).await.unwrap_err();
            assert_eq!(e.code, Some("unsupported"));
            assert!(e.message.contains("\"ACME/app\": \"<path to its clone>\""));
        }
        assert_eq!(f.home, f.setup.home);
    }

    #[tokio::test]
    async fn a_clone_is_a_job_that_ends_approved_with_the_repo_mapped() {
        let r = make_repos();
        // The clone URL is derived from an existing clone's origin; point that at the local bare repo.
        let existing = clone_of(&r.root.join("clones"), "seed", &format!("file://{}", r.origin.display()));
        let mut f = fx(&[("ACME/seed", existing.as_path())]);
        // `git` stand-in: clones from the local bare repo whatever URL is asked for.
        let real = r.origin.to_string_lossy().into_owned();
        f.setup.tools = RepoSetupTools { git: script(&f.root.join("git"), &format!("shift 2; exec git clone --progress '{real}' \"$2\"")), ..Default::default() };
        f.setup.rules = GitRemoteRules::BitbucketDc;
        let job = f.setup.start_clone(&json!({ "project": "ACME", "repo": "newrepo" }), None);
        // The origin doesn't look like a Bitbucket one, so no URL can be derived without a page origin.
        let job = match job {
            Ok(j) => j,
            Err(_) => f.setup.start_clone(&json!({ "project": "ACME", "repo": "newrepo" }), Some("https://bb.example.com")).unwrap(),
        };
        assert_eq!(job["data"]["key"], "ACME/newrepo");
        let done = settled(&f.setup.jobs, job["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "approved", "{done}");
        let target = existing.parent().unwrap().join("newrepo");
        assert!(target.join("README.md").exists());
        assert_eq!(f.setup.config.snapshot()["repos"]["ACME/newrepo"], json!(target));
        assert_eq!(done["result"]["summary"], format!("Cloned ACME/newrepo into {}.", target.display()));
    }

    #[tokio::test]
    async fn a_failed_clone_removes_the_folder_and_says_how_to_do_it_by_hand_and_a_second_request_joins_the_running_job() {
        let mut f = fx(&[]);
        f.setup.tools = RepoSetupTools { git: script(&f.root.join("git"), "mkdir -p \"$4\"; echo 'Cloning into x...' >&2; echo 'Receiving objects: 10% (1/10)' >&2; echo 'fatal: Authentication failed' >&2; sleep 0.3; exit 128"), ..Default::default() };
        let body = json!({ "project": "ACME", "repo": "app" });
        let first = f.setup.start_clone(&body, Some("https://bb.example.com")).unwrap();
        let again = f.setup.start_clone(&json!({ "project": "acme", "repo": "APP" }), Some("https://bb.example.com")).unwrap();
        assert_eq!(first["id"], again["id"], "joined, not a second clone");
        let done = settled(&f.setup.jobs, first["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "failed");
        let err = done["error"].as_str().unwrap();
        assert!(err.starts_with("Cloning ACME/app failed:\nCloning into x...\nfatal: Authentication failed\n\nIf git needs a password"), "{err}");
        assert!(err.contains("  git clone https://bb.example.com/scm/acme/app.git "), "{err}");
        assert!(!Path::new(done["data"]["target"].as_str().unwrap()).exists(), "the half-made folder is removed");
        assert!(f.setup.config.snapshot()["repos"].get("ACME/app").is_none());
    }

    #[tokio::test]
    async fn an_existing_matching_folder_is_used_and_a_different_one_is_refused_and_no_url_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let clones = fs::canonicalize(d.path()).unwrap();
        let known = clone_of(&clones, "known", "https://bb.example.com/scm/ACME/known.git");
        let mine = clone_of(&clones, "app", "https://bb.example.com/scm/ACME/app.git");
        let other = clone_of(&clones, "wrong", "https://bb.example.com/scm/ACME/other.git");
        let f = fx(&[("ACME/known", known.as_path())]);
        let body = json!({ "project": "ACME", "repo": "app" });
        let job = f.setup.start_clone(&body, None).unwrap();
        assert_eq!((job["status"].clone(), job["result"]["summary"].clone()), (json!("approved"), json!(format!("Using {}.", mine.display()))));
        assert_eq!(f.setup.config.snapshot()["repos"]["ACME/app"], json!(mine));
        let e = f.setup.start_clone(&json!({ "project": "ACME", "repo": "wrong" }), None).unwrap_err();
        assert_eq!(e.message, format!("{} already exists and isn't a clone of ACME/wrong. Choose its folder instead.", other.display()));
        let none = fx(&[]);
        assert_eq!(none.setup.start_clone(&body, None).unwrap_err().message, "Couldn't work out a clone URL for ACME/app \u{2014} reload the Bitbucket page and try again.");
    }

    #[tokio::test]
    async fn a_missing_git_fails_the_job_instead_of_hanging() {
        let mut f = fx(&[]);
        f.setup.tools.git = "/no/such/git".into();
        let job = f.setup.start_clone(&json!({ "project": "ACME", "repo": "app" }), Some("https://bb.example.com")).unwrap();
        assert_eq!(job["status"], "failed");
        assert!(job["error"].as_str().unwrap().starts_with("Couldn't run git: "));
        // And the repo isn't stuck "in progress".
        f.setup.tools.git = "/no/such/git".into();
        assert_eq!(f.setup.start_clone(&json!({ "project": "ACME", "repo": "app" }), Some("https://bb.example.com")).unwrap()["status"], "failed");
    }
}
