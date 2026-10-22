//! Opens the file named by a selected stack-trace line in Cursor or VS Code
//! (port of `core/open-location.ts`). The text is untrusted (it comes from a web
//! page): it is only ever parsed into candidates, and a candidate opens only if
//! `git ls-files` lists it in a configured repo, so nothing outside those repos
//! can be opened, and no shell is involved (the URL goes to `open` as one argv
//! word).

use crate::editor_url::{editor_url, pick_editor};
use crate::exec::{self, RunOptions};
use crate::js_text::{js_trim, slice_utf16};
use crate::location_parse::{parse_locations, MAX_TEXT};
use crate::location_resolve::{repo_key_from_page_url, resolve_location, Candidate, Repo, Resolution};
use crate::node_path;
use crate::prereqs::GitRemoteRules;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenLocationErrorCode {
    NoLocation,
    NotFound,
    Ambiguous,
    NoEditor,
    Unsupported,
    GitFailed,
}

impl OpenLocationErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoLocation => "no-location",
            Self::NotFound => "not-found",
            Self::Ambiguous => "ambiguous",
            Self::NoEditor => "no-editor",
            Self::Unsupported => "unsupported",
            Self::GitFailed => "git-failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenLocationError {
    pub code: OpenLocationErrorCode,
    pub message: String,
    pub details: Option<Value>,
}

impl OpenLocationError {
    fn new(code: OpenLocationErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), details: None }
    }
}

impl std::fmt::Display for OpenLocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpenLocationError {}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenedLocation {
    pub repo: String,
    pub path: String,
    pub line: Value,
    pub column: Value,
    pub editor: String,
}

/// Hands a URL to the editor.
#[async_trait]
pub trait UrlOpener: Send + Sync {
    async fn open(&self, url: &str) -> Result<(), String>;
}

/// macOS `open <url>`.
pub struct SystemOpener {
    pub command: String,
}

impl Default for SystemOpener {
    fn default() -> Self {
        Self { command: "open".into() }
    }
}

#[async_trait]
impl UrlOpener for SystemOpener {
    async fn open(&self, url: &str) -> Result<(), String> {
        exec::run(&self.command, &[url.to_string()], RunOptions::default()).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

const FILE_LIST_TTL_MS: i64 = 30_000;
const MAX_FILES: usize = 500_000;
const MAX_CANDIDATES_TRIED: usize = 5;

pub struct OpenLocation {
    pub opener: Arc<dyn UrlOpener>,
    /// Editor ids that are installed.
    pub installed_editors: Vec<String>,
    pub git: GitRemoteRules,
    /// Off only when an opener is injected (tests): the real one is macOS `open`.
    pub require_macos: bool,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    file_lists: Mutex<HashMap<String, (i64, Vec<String>)>>,
}

impl OpenLocation {
    pub fn new(opener: Arc<dyn UrlOpener>, installed_editors: Vec<String>, git: GitRemoteRules, require_macos: bool) -> Self {
        Self { opener, installed_editors, git, require_macos, clock: Arc::new(|| crate::job_files::now_ms() as i64), file_lists: Mutex::new(HashMap::new()) }
    }

    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// `git ls-files` for a repo, cached briefly (a click burst re-uses it).
    async fn tracked_files(&self, root: &str) -> Result<Vec<String>, OpenLocationError> {
        let now = (self.clock)();
        if let Some((at, files)) = self.file_lists.lock().unwrap().get(root) {
            if now - at < FILE_LIST_TTL_MS {
                return Ok(files.clone());
            }
        }
        let out = crate::exec::git(&["ls-files", "-z"], Path::new(root), true, None).await.map_err(|e| OpenLocationError::new(OpenLocationErrorCode::GitFailed, format!("Couldn't list the tracked files of {root} ({e}).")))?;
        if out.code != 0 {
            // Not cached: the user should see the real failure, and a retry should try again.
            let detail = if out.stderr.is_empty() { String::new() } else { format!(": {}", slice_utf16(js_trim(&out.stderr), 200)) };
            return Err(OpenLocationError::new(OpenLocationErrorCode::GitFailed, format!("Couldn't list the tracked files of {root} (git ls-files failed{detail}).")));
        }
        let files: Vec<String> = out.stdout.split('\0').filter(|f| !f.is_empty()).take(MAX_FILES).map(String::from).collect();
        self.file_lists.lock().unwrap().insert(root.to_string(), (now, files.clone()));
        Ok(files)
    }

    async fn configured_repos(&self, config: &Value) -> Result<Vec<Repo>, OpenLocationError> {
        let mut repos = vec![];
        for (key, dir) in config["repos"].as_object().into_iter().flatten() {
            let Some(dir) = dir.as_str() else { continue };
            let root = node_path::resolve(dir);
            if !Path::new(&root).join(".git").exists() {
                continue;
            }
            repos.push(Repo::new(key, &root, self.tracked_files(&root).await?));
        }
        Ok(repos)
    }

    pub async fn open_location(&self, config: &Value, text: Option<&str>, page_url: Option<&str>) -> Result<OpenedLocation, OpenLocationError> {
        use OpenLocationErrorCode::*;
        if self.require_macos && !cfg!(target_os = "macos") {
            return Err(OpenLocationError::new(Unsupported, "Open in editor only works on macOS for now."));
        }
        let text = text.map(|t| slice_utf16(t, MAX_TEXT)).unwrap_or_default();
        let candidates = parse_locations(&text);
        if candidates.is_empty() {
            return Err(OpenLocationError::new(NoLocation, "No file:line location was found in the selected text."));
        }
        let editor = pick_editor(config["reviewEditor"].as_str(), &self.installed_editors).ok_or_else(|| OpenLocationError::new(NoEditor, "Cursor is needed to open a file at a line, and it was not found in /Applications."))?;

        let repos = self.configured_repos(config).await?;
        if repos.is_empty() {
            return Err(OpenLocationError::new(NotFound, "No repositories are set up yet \u{2014} map one in \u{2699} Settings first."));
        }
        let keys: Vec<String> = repos.iter().map(|r| r.key.clone()).collect();
        let hint = repo_key_from_page_url(page_url, &keys, self.git);

        let mut ambiguous: Option<Vec<(String, String)>> = None;
        for c in candidates.iter().take(MAX_CANDIDATES_TRIED) {
            let candidate = Candidate { path: c.path.clone(), line: json!(c.line), column: json!(c.column) };
            match resolve_location(&candidate, &repos, hint.as_deref()) {
                Resolution::Found { repo_key, root, rel, abs, line, column } => {
                    if !stays_inside_repo(&root, &abs) {
                        continue; // a symlink pointing out of the repo
                    }
                    let url = editor_url(editor, &abs, Some(&line), Some(&column)).map_err(|e| OpenLocationError::new(NoEditor, e))?;
                    self.opener.open(&url).await.map_err(|e| OpenLocationError::new(NotFound, e))?;
                    return Ok(OpenedLocation { repo: repo_key, path: rel, line, column, editor: editor.to_string() });
                }
                Resolution::Ambiguous { matches } if ambiguous.is_none() => ambiguous = Some(matches),
                _ => {}
            }
        }
        if let Some(matches) = ambiguous {
            let list = matches.iter().map(|(k, r)| format!("{k}: {r}")).collect::<Vec<_>>().join("; ");
            return Err(OpenLocationError { code: Ambiguous, message: format!("That file name matches several files ({list})."), details: Some(json!({ "matches": matches.iter().map(|(k, r)| json!({ "repoKey": k, "rel": r })).collect::<Vec<_>>() })) });
        }
        Err(OpenLocationError::new(NotFound, "None of the files in the selected text is tracked in your set-up repositories."))
    }
}

/// True when the file, after following symlinks, is still inside the repo root (both really exist).
fn stays_inside_repo(root: &str, abs: &str) -> bool {
    let (Ok(real_root), Ok(real_abs)) = (fs::canonicalize(root), fs::canonicalize(abs)) else { return false };
    let (real_root, real_abs) = (real_root.to_string_lossy().into_owned(), real_abs.to_string_lossy().into_owned());
    real_abs.starts_with(&if real_root.ends_with('/') { real_root } else { format!("{real_root}/") })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git_cli::testing::git as sh;
    use std::path::PathBuf;

    struct Recorder(Mutex<Vec<String>>);
    #[async_trait]
    impl UrlOpener for Recorder {
        async fn open(&self, url: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(url.to_string());
            Ok(())
        }
    }

    enum F<'a> {
        File(&'a str),
        Link(String),
    }

    fn make_repo(files: &[(&str, F)]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap().join("repo");
        fs::create_dir(&root).unwrap();
        sh(&root, &["init", "-q"]);
        for (rel, f) in files {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            match f {
                F::File(c) => fs::write(&p, c).unwrap(),
                F::Link(t) => std::os::unix::fs::symlink(t, &p).unwrap(),
            }
        }
        sh(&root, &["add", "-A"]);
        (d, root)
    }

    fn open_location(rec: &Arc<Recorder>) -> OpenLocation {
        OpenLocation::new(rec.clone(), vec!["vscode".into(), "cursor".into()], GitRemoteRules::BitbucketDc, false)
    }
    fn recorder() -> Arc<Recorder> {
        Arc::new(Recorder(Mutex::new(vec![])))
    }
    fn config(root: &Path) -> Value {
        json!({ "repos": { "P/r": root } })
    }

    #[tokio::test]
    async fn opens_a_tracked_file_inside_the_repo() {
        let (_d, root) = make_repo(&[("src/a.ts", F::File("x"))]);
        let rec = recorder();
        let r = open_location(&rec).open_location(&config(&root), Some("src/a.ts:3"), None).await.unwrap();
        assert_eq!((r.repo.as_str(), r.path.as_str(), r.line, r.editor.as_str()), ("P/r", "src/a.ts", json!(3), "cursor"));
        assert_eq!(rec.0.lock().unwrap().as_slice(), [format!("cursor://file{}/src/a.ts:3:1", root.display())]);
    }

    #[tokio::test]
    async fn a_tracked_symlink_that_points_outside_the_repo_is_treated_as_not_found() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.ts"), "x").unwrap();
        let (_d, root) = make_repo(&[("src/link.ts", F::Link(outside.path().join("secret.ts").to_string_lossy().into_owned()))]);
        let rec = recorder();
        let e = open_location(&rec).open_location(&config(&root), Some("src/link.ts:1"), None).await.unwrap_err();
        assert_eq!(e.code, OpenLocationErrorCode::NotFound);
        assert!(rec.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_git_ls_files_surfaces_as_git_failed_and_is_not_cached() {
        let (_d, root) = make_repo(&[("src/a.ts", F::File("x"))]);
        let (git_dir, moved) = (root.join(".git"), root.join(".git.moved"));
        // Leave a .git entry that exists but is not a repository.
        fs::rename(&git_dir, &moved).unwrap();
        fs::create_dir(&git_dir).unwrap();
        let rec = recorder();
        let ol = open_location(&rec);
        let e = ol.open_location(&config(&root), Some("src/a.ts:1"), None).await.unwrap_err();
        assert_eq!(e.code, OpenLocationErrorCode::GitFailed);
        assert!(e.message.starts_with(&format!("Couldn't list the tracked files of {} (git ls-files failed", root.display())), "{}", e.message);
        // Repair: the next call must retry rather than reuse a cached empty list.
        fs::remove_dir_all(&git_dir).unwrap();
        fs::rename(&moved, &git_dir).unwrap();
        assert_eq!(ol.open_location(&config(&root), Some("src/a.ts:1"), None).await.unwrap().path, "src/a.ts");
    }

    #[tokio::test]
    async fn the_file_list_is_cached_briefly_then_refetched() {
        let (_d, root) = make_repo(&[("a.ts", F::File("x"))]);
        let now = Arc::new(Mutex::new(0i64));
        let n = now.clone();
        let rec = recorder();
        let ol = open_location(&rec).with_clock(move || *n.lock().unwrap());
        ol.open_location(&config(&root), Some("a.ts:1"), None).await.unwrap();
        fs::write(root.join("b.ts"), "x").unwrap();
        sh(&root, &["add", "b.ts"]);
        assert_eq!(ol.open_location(&config(&root), Some("b.ts:1"), None).await.unwrap_err().code, OpenLocationErrorCode::NotFound, "a cached list doesn't know b.ts yet");
        *now.lock().unwrap() = 31_000;
        assert_eq!(ol.open_location(&config(&root), Some("b.ts:1"), None).await.unwrap().path, "b.ts");
    }

    #[tokio::test]
    async fn the_failure_modes_have_their_own_codes_and_messages() {
        let (_d, root) = make_repo(&[("a/x.ts", F::File("1")), ("b/x.ts", F::File("2"))]);
        let rec = recorder();
        let ol = open_location(&rec);
        let code = |r: Result<OpenedLocation, OpenLocationError>| r.unwrap_err();
        assert_eq!(code(ol.open_location(&config(&root), Some("no location here"), None).await).code, OpenLocationErrorCode::NoLocation);
        let none = OpenLocation::new(rec.clone(), vec![], GitRemoteRules::BitbucketDc, false);
        assert_eq!(code(none.open_location(&config(&root), Some("a/x.ts:1"), None).await).code, OpenLocationErrorCode::NoEditor);
        assert_eq!(code(ol.open_location(&json!({}), Some("a/x.ts:1"), None).await).message, "No repositories are set up yet \u{2014} map one in \u{2699} Settings first.");
        let amb = code(ol.open_location(&config(&root), Some("x.ts:1"), None).await);
        assert_eq!(amb.code, OpenLocationErrorCode::Ambiguous);
        assert_eq!(amb.message, "That file name matches several files (P/r: a/x.ts; P/r: b/x.ts).");
        assert_eq!(amb.details.unwrap()["matches"][1], json!({ "repoKey": "P/r", "rel": "b/x.ts" }));
        assert_eq!(code(ol.open_location(&config(&root), Some("nope/zzz.ts:1"), None).await).message, "None of the files in the selected text is tracked in your set-up repositories.");
        // A folder that isn't a git repository is skipped, not an error.
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(code(ol.open_location(&json!({ "repos": { "P/p": plain.path() } }), Some("a/x.ts:1"), None).await).code, OpenLocationErrorCode::NotFound);
        assert!(rec.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_configured_editor_wins_when_installed_and_opening_is_macos_only_with_the_real_opener() {
        let (_d, root) = make_repo(&[("a.ts", F::File("x"))]);
        let rec = recorder();
        let ol = open_location(&rec);
        let cfg = json!({ "repos": { "P/r": root }, "reviewEditor": "vscode" });
        assert_eq!(ol.open_location(&cfg, Some("a.ts:2:5"), None).await.unwrap().editor, "vscode");
        assert!(rec.0.lock().unwrap()[0].starts_with("vscode://file"));
        if !cfg!(target_os = "macos") {
            let real = OpenLocation::new(Arc::new(SystemOpener::default()), vec!["cursor".into()], GitRemoteRules::BitbucketDc, true);
            assert_eq!(real.open_location(&cfg, Some("a.ts:1"), None).await.unwrap_err().code, OpenLocationErrorCode::Unsupported);
        }
    }
}
