//! Prerequisite checks and git-remote rules shared by setup (the interactive
//! wizard) and the server's fail-fast startup check (port of
//! `core/prereqs.js`).
//!
//! Dropped from the Node build: `checkNodeVersion`/`nodeMeetsMinimum` (there is
//! no Node to be too old any more).
//!
//! The git provider's naming rules are an explicit value
//! ([`GitRemoteRules`]) so tests can exercise each provider without touching
//! the active profile; [`git_remote_rules`] picks the profile's.

use crate::environment;
use crate::js_text::{js_trim, locale_compare};
use crate::node_path;
use crate::uri::encode_uri_component;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub ok: bool,
    pub message: String,
}

fn check(ok: bool, message: impl Into<String>) -> Check {
    Check { ok, message: message.into() }
}

fn run_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn check_git() -> Check {
    match run_stdout("git", &["--version"]) {
        Some(out) => check(true, format!("{} OK.", js_trim(&out))),
        None => check(false, "git not found on PATH. Install git (on macOS: `xcode-select --install`) and try again."),
    }
}

pub fn check_claude_cli() -> Check {
    match run_stdout("claude", &["--version"]) {
        Some(out) => check(true, format!("claude CLI {} found.", js_trim(&out))),
        None => check(false, "claude CLI not found on PATH. Install Claude Code first: https://claude.com/claude-code"),
    }
}

/// `claude auth status` gets a timeout with SIGKILL: a hung `claude` process
/// (locked keychain, stuck network prompt) must not block this check forever,
/// since callers run it synchronously. SIGKILL rather than SIGTERM because
/// `claude` may be ignoring signals in whatever state it's stuck in.
pub const CLAUDE_AUTH_TIMEOUT: Duration = Duration::from_secs(20);

pub fn check_claude_auth() -> Check {
    check_claude_auth_with("claude", CLAUDE_AUTH_TIMEOUT)
}

/// As [`check_claude_auth`] with the program and timeout given (the timeout is
/// injectable purely so tests can exercise that path quickly, with a stand-in
/// `claude`).
pub fn check_claude_auth_with(program: &str, timeout: Duration) -> Check {
    let mut cmd = Command::new(program);
    cmd.args(["auth", "status"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // Its own process group, so a timeout takes down whatever it started too:
    // a grandchild holding the output pipe open would otherwise keep us waiting.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return check(false, format!("Could not check claude auth status: {e}")),
    };
    // Read stdout on a thread so a full pipe can't stall the child while we poll.
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => {
                #[cfg(unix)]
                // SAFETY: SIGKILL to the process group we just created for this child.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return check(false, format!("Could not check claude auth status: {e}")),
        }
    };
    let out = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    if status.is_none() && js_trim(&out).is_empty() {
        return check(
            false,
            format!(
                "claude auth status did not respond within {}s \u{2014} it may be stuck (a locked keychain, a stalled network prompt). Try running it by hand.",
                timeout.as_secs_f64()
            ),
        );
    }
    // `claude auth status` exits non-zero when simply logged out, still printing
    // its normal {"loggedIn": false, ...} JSON to stdout, so a non-zero exit with
    // output is a status to parse rather than an error.
    if js_trim(&out).is_empty() {
        return check(false, "Could not check claude auth status: claude exited without printing a status");
    }
    match serde_json::from_str::<Value>(&out) {
        Ok(status) if crate::js_text::js_truthy(status.get("loggedIn")) => {
            let who = status.get("email").and_then(Value::as_str).filter(|e| !e.is_empty()).unwrap_or("unknown user");
            check(true, format!("claude CLI logged in as {who}."))
        }
        Ok(_) => check(false, "claude CLI is installed but not logged in. Run `claude auth login`."),
        Err(e) => check(false, format!("Could not parse claude auth status output: {e}")),
    }
}

/// Whether nothing is listening on `127.0.0.1:port`.
pub fn check_port_free(port: u16) -> Check {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => check(true, format!("Port {port} is free.")),
        Err(_) => check(false, format!("Port {port} is already in use.")),
    }
}

fn git_ok(args: &[&str]) -> Option<String> {
    run_stdout("git", args)
}

pub fn check_repo_path(repo_path: &str) -> Check {
    if !Path::new(repo_path).exists() {
        return check(false, format!("{repo_path} does not exist."));
    }
    if !Path::new(&format!("{repo_path}/.git")).exists() {
        return check(false, format!("{repo_path} is not a git repository (no .git found)."));
    }
    if git_ok(&["-C", repo_path, "remote", "get-url", "origin"]).is_none() {
        return check(false, format!("{repo_path} has no \"origin\" remote configured."));
    }
    // If the shallow probe fails, the origin check above would have failed first.
    if git_ok(&["-C", repo_path, "rev-parse", "--is-shallow-repository"]).is_some_and(|s| js_trim(&s) == "true") {
        return check(
            false,
            format!("{repo_path} is a shallow clone \u{2014} run \"git fetch --unshallow\" there first (a shallow clone silently breaks merge-conflict detection)."),
        );
    }
    check(true, format!("{repo_path} OK."))
}

pub fn get_origin_url(repo_path: &str) -> Option<String> {
    git_ok(&["-C", repo_path, "remote", "get-url", "origin"]).map(|s| js_trim(&s).to_string())
}

/// True if `origin_url` looks like it points at exactly this project/repo (a
/// Bitbucket project and repo slug, or a GitHub owner and repo). Bitbucket
/// Server's own clone URLs lowercase the project key in the path, and GitHub
/// names are case-insensitive, so the comparison is case-insensitive. The
/// ".git" suffix is optional: `git clone https://github.com/o/r` leaves an
/// origin without it.
pub fn origin_matches_project_repo(origin_url: Option<&str>, project: &str, repo: &str) -> bool {
    let Some(origin) = origin_url.filter(|o| !o.is_empty()) else { return false };
    let pattern = format!(r"[:/]{}/{}(?:\.git)?/?$", regex::escape(project), regex::escape(repo));
    RegexBuilder::new(&pattern).case_insensitive(true).build().is_ok_and(|re| re.is_match(js_trim(origin)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedOrigin {
    pub project: String,
    pub repo: String,
    pub key: String,
}

fn ci(pattern: &str) -> Regex {
    RegexBuilder::new(pattern).case_insensitive(true).build().unwrap()
}

static BB_HTTP_SCHEME: LazyLock<Regex> = LazyLock::new(|| ci(r"^https?:"));
static BB_HTTP: LazyLock<Regex> = LazyLock::new(|| ci(r"/scm/([^/]+)/([^/]+?)(?:\.git)?/?$"));
static BB_SSH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[:/]([^/:]+)/([^/]+?)(?:\.git)?/?$").unwrap());
static GH_OR_GL: LazyLock<Regex> = LazyLock::new(|| ci(r"github\.com|gitlab\.com"));
static GH_SCM: LazyLock<Regex> = LazyLock::new(|| ci(r"/scm/"));
static GH_URL_SCHEME: LazyLock<Regex> = LazyLock::new(|| ci(r"^[a-z][a-z0-9+.-]*://"));
static GH_URL: LazyLock<Regex> = LazyLock::new(|| ci(r"^[a-z][a-z0-9+.-]*://(?:[^/@]+@)?[^/]+/([^/]+)/([^/]+?)(?:\.git)?/?$"));
static GH_SCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:[^@/]+@)?[^:/]+:([^/]+)/([^/]+?)(?:\.git)?/?$").unwrap());

/// Parses a Bitbucket Server origin URL into the "PROJECT/repo" key the
/// extension sends (project keys are upper-case in PR URLs, lower-case in
/// clone URLs). Accepts the HTTPS shape (`.../scm/acme/sample-app.git`) and the
/// SSH shapes (`ssh://git@host:7999/acme/sample-app.git`,
/// `git@host:acme/sample-app.git`). `None` for anything else: personal (~user)
/// repos, and GitHub/GitLab remotes, which would otherwise parse as a
/// plausible-looking owner/repo.
pub fn parse_bitbucket_origin(origin_url: Option<&str>) -> Option<ParsedOrigin> {
    let url = js_trim(origin_url.filter(|o| !o.is_empty())?);
    if GH_OR_GL.is_match(url) {
        return None;
    }
    let caps = if BB_HTTP_SCHEME.is_match(url) { BB_HTTP.captures(url) } else { BB_SSH.captures(url) }?;
    let project = caps.get(1)?.as_str();
    if project.starts_with('~') {
        return None;
    }
    let (project, repo) = (project.to_uppercase(), caps.get(2)?.as_str().to_string());
    Some(ParsedOrigin { key: format!("{project}/{repo}"), project, repo })
}

/// Parses a GitHub (or GitHub Enterprise) origin URL into its "owner/repo" key.
/// Accepts the HTTPS shape (with or without ".git"), `git@github.com:o/r.git`
/// and `ssh://git@host[:port]/o/r.git`. `None` for anything else, and for a
/// Bitbucket "/scm/" path.
pub fn parse_github_origin(origin_url: Option<&str>) -> Option<ParsedOrigin> {
    let url = js_trim(origin_url.filter(|o| !o.is_empty())?);
    if GH_SCM.is_match(url) {
        return None;
    }
    let caps = if GH_URL_SCHEME.is_match(url) { GH_URL.captures(url) } else { GH_SCP.captures(url) }?;
    let (project, repo) = (caps.get(1)?.as_str().to_string(), caps.get(2)?.as_str().to_string());
    Some(ParsedOrigin { key: format!("{project}/{repo}"), project, repo })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrRef {
    pub project: String,
    pub repo: String,
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoRef {
    pub project: String,
    pub repo: String,
}

/// How each git provider names things: reading a clone's origin, where a fresh
/// clone comes from, and the shape of a pull request's web address and of the
/// key it is filed under in history and scope keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRemoteRules {
    BitbucketDc,
    Github,
}

static BB_PR_URL: LazyLock<Regex> = LazyLock::new(|| ci(r"/projects/([^/]+)/repos/([^/?#]+)/pull-requests/([0-9]+)"));
static BB_REPO_URL: LazyLock<Regex> = LazyLock::new(|| ci(r"/projects/([^/]+)/repos/([^/?#]+)"));
static GH_API_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(https?://[^/]+)/api/v3").unwrap());
static GH_PR_URL: LazyLock<Regex> = LazyLock::new(|| ci(r"^(?:[a-z]+://[^/]+)?/([^/]+)/([^/?#]+)/pull/([0-9]+)"));
static GH_REPO_URL: LazyLock<Regex> = LazyLock::new(|| ci(r"^(?:[a-z]+://[^/]+)?/([^/]+)/([^/?#]+)"));
static CLONE_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^/:]+/[^/]+?(?:\.git)?/?$").unwrap());

impl GitRemoteRules {
    /// The rules for a provider id from the profile's `sites` (`bitbucket-dc`, `github`).
    pub fn for_provider(provider: &str) -> Option<Self> {
        match provider {
            "bitbucket-dc" => Some(Self::BitbucketDc),
            "github" => Some(Self::Github),
            _ => None,
        }
    }

    pub fn key_prefix(self) -> &'static str {
        match self {
            Self::BitbucketDc => "bitbucket",
            Self::Github => "github",
        }
    }

    pub fn parse_origin(self, url: Option<&str>) -> Option<ParsedOrigin> {
        match self {
            Self::BitbucketDc => parse_bitbucket_origin(url),
            Self::Github => parse_github_origin(url),
        }
    }

    pub fn clone_url_tail(self, project: &str, repo: &str) -> String {
        match self {
            Self::BitbucketDc => format!("{}/{repo}.git", project.to_lowercase()),
            Self::Github => format!("{project}/{repo}.git"),
        }
    }

    pub fn page_clone_url(self, page_origin: &str, project: &str, repo: &str) -> String {
        let origin = page_origin.strip_suffix('/').unwrap_or(page_origin);
        match self {
            Self::BitbucketDc => format!("{origin}/scm/{}/{repo}.git", project.to_lowercase()),
            Self::Github => format!("{origin}/{project}/{repo}.git"),
        }
    }

    /// `<host>:<project>/<repo>#<n>` (Bitbucket upper-cases the project, GitHub lower-cases both).
    pub fn pr_key(self, project: &str, repo: &str, id: &str) -> String {
        match self {
            Self::BitbucketDc => format!("bitbucket:{}/{}#{id}", project.to_uppercase(), repo.to_lowercase()),
            Self::Github => format!("github:{}/{}#{id}", project.to_lowercase(), repo.to_lowercase()),
        }
    }

    /// The path of a PR page.
    pub fn pr_path(self, project: &str, repo: &str, id: u64) -> String {
        let (p, r) = (encode_uri_component(project), encode_uri_component(repo));
        match self {
            Self::BitbucketDc => format!("/projects/{p}/repos/{r}/pull-requests/{id}"),
            Self::Github => format!("/{p}/{r}/pull/{id}"),
        }
    }

    /// `{ project, repo, id }` of a PR page's address, or `None`.
    pub fn parse_pr_url(self, url: &str) -> Option<PrRef> {
        let normalized;
        let (re, text): (&Regex, &str) = match self {
            Self::BitbucketDc => (&BB_PR_URL, url),
            Self::Github => {
                normalized = GH_API_PREFIX.replace(url, "$1").into_owned();
                (&GH_PR_URL, &normalized)
            }
        };
        let m = re.captures(text)?;
        Some(PrRef { project: m[1].to_string(), repo: m[2].to_string(), id: m[3].parse().ok()? })
    }

    /// `{ project, repo }` of any page of a repository, or `None` (for addresses in ticket text).
    pub fn parse_repo_url(self, url: &str) -> Option<RepoRef> {
        let re = match self {
            Self::BitbucketDc => &BB_REPO_URL,
            Self::Github => &GH_REPO_URL,
        };
        let m = re.captures(url)?;
        Some(RepoRef { project: m[1].to_string(), repo: m[2].to_string() })
    }
}

/// The remote rules of this distribution's git provider (the profile's `sites`);
/// Bitbucket's when it names none.
pub fn git_remote_rules() -> GitRemoteRules {
    environment::current()
        .sites
        .iter()
        .find(|s| s.kind == "git")
        .and_then(|s| GitRemoteRules::for_provider(&s.provider))
        .unwrap_or(GitRemoteRules::BitbucketDc)
}

/// Reads `remote "origin"`'s url straight from `.git/config`: much faster than
/// spawning git once per candidate while scanning whole folders.
pub fn read_origin_from_git_config(repo_dir: &Path) -> Option<String> {
    static SECTION_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*\[").unwrap());
    static ORIGIN_SECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^remote\s+"origin"\s*\]"#).unwrap());
    static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?mR)^\s*url\s*=\s*([^\n\r\x{2028}\x{2029}]+)$").unwrap());
    let text = String::from_utf8_lossy(&std::fs::read(repo_dir.join(".git").join("config")).ok()?).into_owned();
    let section = SECTION_START.split(&text).find(|s| ORIGIN_SECTION.is_match(s))?;
    Some(js_trim(&URL.captures(section)?[1]).to_string())
}

/// Folders people commonly keep clones in: searched (one level deep) by setup's
/// repo discovery and by runtime inference for an unmapped repo.
pub fn default_search_roots(home: &Path) -> Vec<PathBuf> {
    ["gitviews", "git", "src", "code", "projects", "workspace", "repos", "dev", "Development", "Documents/GitHub", "Documents/git"]
        .iter()
        .map(|d| home.join(d))
        .filter(|d| d.exists())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalClone {
    pub key: String,
    pub path: String,
}

/// Every clone of the rules' provider found directly inside `roots` (one level
/// deep), sorted by key. Only real clones count: a directory whose `.git` is a
/// *file* is a linked worktree of some other clone, and `*.worktrees` folders
/// are this tool's own review worktrees.
pub fn discover_local_clones(roots: &[PathBuf], rules: GitRemoteRules) -> Vec<LocalClone> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut seen_roots: Vec<&PathBuf> = Vec::new();
    for root in roots {
        if seen_roots.contains(&root) {
            continue;
        }
        seen_roots.push(root);
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for name in names {
            if name.starts_with('.') || name.ends_with(".worktrees") || name == "node_modules" {
                continue;
            }
            let dir = root.join(&name);
            if !std::fs::metadata(dir.join(".git")).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            if let Some(parsed) = rules.parse_origin(read_origin_from_git_config(&dir).as_deref()) {
                if !found.iter().any(|(k, _)| *k == parsed.key) {
                    found.push((parsed.key, dir));
                }
            }
        }
    }
    found.sort_by(|a, b| locale_compare(&a.0, &b.0));
    found.into_iter().map(|(key, dir)| LocalClone { key, path: dir.to_string_lossy().into_owned() }).collect()
}

/// True for a project key / owner / repo name that's safe to use as a path
/// segment and in a clone URL. A single leading dot is allowed (GitHub's
/// ".github" repository), never "." or ".." or a leading dash.
///
/// Equivalent to `/^(?:[A-Za-z0-9]|\.[A-Za-z0-9])[A-Za-z0-9._-]*$/` plus no
/// ".." anywhere and not ".git" in any case, written without a regex: the
/// pattern is plain ASCII, so a hand-written check is exact.
pub fn is_safe_repo_segment(value: &str) -> bool {
    let b = value.as_bytes();
    let head_ok = match b {
        [] => false,
        [c, ..] if c.is_ascii_alphanumeric() => true,
        [b'.', c, ..] if c.is_ascii_alphanumeric() => true,
        _ => false,
    };
    head_ok
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && !value.contains("..")
        && !value.eq_ignore_ascii_case(".git")
}

/// The same check over an untyped JSON value; anything but a string is unsafe.
pub fn is_safe_repo_segment_value(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::String(s)) if is_safe_repo_segment(s))
}


/// Clone URL for PROJECT/repo. Prefers the shape of an existing clone's origin
/// (so SSH users get SSH, with the same host/port/user) and falls back to the
/// git host's page origin over HTTPS. `None` if neither is available.
pub fn derive_clone_url<S: AsRef<str>>(existing_origins: &[S], page_origin: Option<&str>, project: &str, repo: &str, rules: GitRemoteRules) -> Option<String> {
    let tail = rules.clone_url_tail(project, repo);
    for origin in existing_origins {
        if rules.parse_origin(Some(origin.as_ref())).is_none() {
            continue;
        }
        return Some(CLONE_TAIL.replace(js_trim(origin.as_ref()), regex::NoExpand(&tail)).into_owned());
    }
    page_origin.filter(|p| !p.is_empty()).map(|p| rules.page_clone_url(p, project, repo))
}

/// Where a new clone should go: the folder most of the mapped clones share,
/// else the first common clone folder that exists, else `~/gitviews`.
pub fn clone_root(existing_repos: &[PathBuf], search_roots: &[PathBuf], home: &Path) -> PathBuf {
    let mut counts: Vec<(PathBuf, usize)> = Vec::new();
    for p in existing_repos {
        let parent = PathBuf::from(node_path::dirname(&p.to_string_lossy()));
        match counts.iter_mut().find(|(d, _)| *d == parent) {
            Some((_, n)) => *n += 1,
            None => counts.push((parent, 1)),
        }
    }
    // A stable sort by count keeps the first-seen folder among ties.
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    counts.into_iter().next().map(|(d, _)| d).or_else(|| search_roots.first().cloned()).unwrap_or_else(|| home.join("gitviews"))
}

/// Given the existing repos map (project/repo -> local path), try to find where
/// an unmapped project/repo might already be cloned locally, by checking the
/// sibling of every already-known clone's directory, plus any `extra_roots`.
/// Only ever returns a candidate whose *origin* remote is confirmed (via
/// [`origin_matches_project_repo`]) to actually point at this exact
/// project/repo: directory existence alone is never enough to auto-adopt a
/// mapping.
pub fn infer_repo_path(existing_repos: &[PathBuf], project: &str, repo: &str, extra_roots: &[PathBuf]) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for p in existing_repos {
        roots.push(PathBuf::from(node_path::dirname(&p.to_string_lossy())));
    }
    roots.extend(extra_roots.iter().cloned());
    let mut tried: Vec<&PathBuf> = Vec::new();
    for root in &roots {
        if tried.contains(&root) {
            continue;
        }
        tried.push(root);
        let candidate = root.join(repo);
        let c = candidate.to_string_lossy();
        if check_repo_path(&c).ok && origin_matches_project_repo(get_origin_url(&c).as_deref(), project, repo) {
            return Some(candidate);
        }
    }
    None
}

/// The names the feature descriptors' `requiredChecks` use, so setup and the
/// server run only the checks the *enabled* features actually need (e.g. skip
/// requiring the `claude` CLI if no enabled feature uses it).
pub fn run_check_by_name(name: &str) -> Option<Check> {
    match name {
        "git" => Some(check_git()),
        "claudeCli" => Some(check_claude_cli()),
        "claudeAuth" => Some(check_claude_auth()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Editor {
    pub id: &'static str,
    pub label: &'static str,
}

/// Which of review-in-editor's supported editors are actually usable on this
/// machine, in preference order: used both to default `reviewEditor` at setup
/// time and to pick a fallback at runtime. Not a hard prerequisite (a missing
/// editor just makes this list shorter), so a list rather than a [`Check`].
pub fn detect_installed_editors() -> Vec<Editor> {
    let mut found = Vec::new();
    if !cfg!(target_os = "macos") {
        return found;
    }
    // Claude Code first: it's the default editor offered by the setup wizard
    // (and the runtime fallback) whenever it's on PATH.
    if run_stdout("claude", &["--version"]).is_some() {
        found.push(Editor { id: "claude-code", label: "Claude Code (opens a new terminal)" });
    }
    if Path::new("/Applications/Cursor.app").exists() {
        found.push(Editor { id: "cursor", label: "Cursor" });
    }
    // VS Code is deliberately not detected: it's hidden for now.
    found
}

/// The editor to use: `configured` when it's one of the supported ids
/// (claude-code, cursor), else the first installed one. A saved "vscode" from
/// before it was hidden counts as unset.
pub fn resolve_review_editor(configured: Option<&str>) -> Option<&'static str> {
    match configured {
        Some("claude-code") => Some("claude-code"),
        Some("cursor") => Some("cursor"),
        _ => detect_installed_editors().first().map(|e| e.id),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    /// A real (but network-free) git repo: `git init` plus a fake origin remote.
    fn make_fake_clone(dir: &Path, origin: &str) {
        fs::create_dir_all(dir).unwrap();
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
            assert!(Command::new("git").args(&args).current_dir(dir).output().unwrap().status.success());
        }
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn git_is_found_and_a_missing_repo_or_non_repo_fails_with_a_clear_message() {
        assert!(check_git().ok);
        let missing = check_repo_path("/definitely/not/a/real/path/xyz-does-not-exist");
        assert!(!missing.ok && missing.message.contains("does not exist"));
        let dir = tempfile::tempdir().unwrap();
        let not_repo = check_repo_path(&s(dir.path()));
        assert!(!not_repo.ok && not_repo.message.contains("not a git repository"));
        let no_origin = tempfile::tempdir().unwrap();
        assert!(Command::new("git").args(["init", "-q"]).current_dir(no_origin.path()).output().unwrap().status.success());
        assert!(check_repo_path(&s(no_origin.path())).message.contains("no \"origin\" remote"));
        let ok = tempfile::tempdir().unwrap();
        make_fake_clone(ok.path(), "https://github.com/o/r.git");
        assert_eq!(check_repo_path(&s(ok.path())), check(true, format!("{} OK.", s(ok.path()))));
        assert_eq!(get_origin_url(&s(ok.path())).as_deref(), Some("https://github.com/o/r.git"));
        assert_eq!(get_origin_url("/no/such"), None);
    }

    fn fake_claude(dir: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("claude");
        fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        s(&bin)
    }

    #[test]
    fn a_timed_out_claude_auth_status_is_a_failed_check_with_an_actionable_message() {
        let dir = tempfile::tempdir().unwrap();
        // Ignores TERM but not KILL, and never produces stdout: only a SIGKILL ends it before the 5 s sleep.
        let bin = fake_claude(dir.path(), "trap \"\" TERM\nsleep 5");
        let started = Instant::now();
        let r = check_claude_auth_with(&bin, Duration::from_millis(200));
        assert!(!r.ok && r.message.contains("did not respond within 0.2s"), "{}", r.message);
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(check_claude_auth_with("/no/such/claude", Duration::from_secs(1)).message.starts_with("Could not check claude auth status:"));
    }

    #[test]
    fn claude_auth_status_json_is_read_whether_or_not_the_exit_code_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let logged_in = fake_claude(dir.path(), "echo '{\"loggedIn\":true,\"email\":\"me@example.com\"}'");
        assert_eq!(check_claude_auth_with(&logged_in, Duration::from_secs(5)), check(true, "claude CLI logged in as me@example.com."));
        let no_email = fake_claude(dir.path(), "echo '{\"loggedIn\":true}'");
        assert_eq!(check_claude_auth_with(&no_email, Duration::from_secs(5)).message, "claude CLI logged in as unknown user.");
        let out = fake_claude(dir.path(), "echo '{\"loggedIn\":false}'; exit 1");
        assert_eq!(check_claude_auth_with(&out, Duration::from_secs(5)), check(false, "claude CLI is installed but not logged in. Run `claude auth login`."));
        let junk = fake_claude(dir.path(), "echo not-json");
        assert!(check_claude_auth_with(&junk, Duration::from_secs(5)).message.starts_with("Could not parse claude auth status output:"));
        let silent = fake_claude(dir.path(), "exit 3");
        assert!(check_claude_auth_with(&silent, Duration::from_secs(5)).message.starts_with("Could not check claude auth status:"));
    }

    #[test]
    fn port_checks_report_free_and_occupied_ports() {
        assert!(check_port_free(58_234).ok);
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let r = check_port_free(port);
        assert!(!r.ok && r.message.contains("already in use"));
    }

    #[test]
    fn origin_matching_is_case_insensitive_literal_and_tolerates_a_missing_git_suffix() {
        let m = |o: &str, p: &str, r: &str| origin_matches_project_repo(Some(o), p, r);
        assert!(m("https://bitbucket.example.com/scm/acme/sample-app.git", "ACME", "sample-app"));
        assert!(m("ssh://git@bitbucket.example.com/acme/sample-app.git", "ACME", "sample-app"));
        assert!(!m("https://bitbucket.example.com/scm/acme/sample-service.git", "ACME", "sample-app"));
        assert!(!m("https://bitbucket.example.com/scm/other/sample-app.git", "ACME", "sample-app"));
        assert!(!origin_matches_project_repo(None, "ACME", "sample-app") && !origin_matches_project_repo(Some(""), "o", "r"));
        assert!(m("https://github.com/Octo/hello", "octo", "hello") && m("https://github.com/Octo/hello.git", "octo", "hello"));
        assert!(m("git@github.com:octo/hello.git", "OCTO", "hello") && m("https://github.com/octo/hello/", "octo", "hello"));
        assert!(!m("https://github.com/octo/hello-world", "octo", "hello"), "a longer name is another repo");
        assert!(!m("https://github.com/octo/hello", "octo", "hel.o"), "the dot is a dot, not a wildcard");
        assert!(!m("https://github.com/o.cto/hello", "oXcto", "hello"));
        assert!(m("  https://github.com/octo/hello  ", "octo", "hello"), "the origin is trimmed");
    }

    #[test]
    fn infer_repo_path_adopts_only_a_sibling_whose_origin_matches() {
        let root = tempfile::tempdir().unwrap();
        make_fake_clone(&root.path().join("sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
        make_fake_clone(&root.path().join("sample-service"), "https://bitbucket.example.com/scm/acme/sample-service.git");
        let known = [root.path().join("sample-app")];
        assert_eq!(infer_repo_path(&known, "ACME", "sample-service", &[]), Some(root.path().join("sample-service")));
        // A directory named like the repo but cloned from something unrelated is not adopted.
        let other = tempfile::tempdir().unwrap();
        make_fake_clone(&other.path().join("sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
        make_fake_clone(&other.path().join("sample-service"), "https://bitbucket.example.com/scm/other/unrelated-fork.git");
        assert_eq!(infer_repo_path(&[other.path().join("sample-app")], "ACME", "sample-service", &[]), None);
        assert_eq!(infer_repo_path(&known, "ACME", "nonexistent-repo", &[]), None);
        // extra roots are searched too.
        assert_eq!(infer_repo_path(&[], "ACME", "sample-app", &[]), None);
        assert_eq!(infer_repo_path(&[], "ACME", "sample-app", &[root.path().to_path_buf()]), Some(root.path().join("sample-app")));
    }

    #[test]
    fn parse_bitbucket_origin_handles_https_and_both_ssh_shapes_and_rejects_the_rest() {
        let want = Some(ParsedOrigin { project: "ACME".into(), repo: "sample-app".into(), key: "ACME/sample-app".into() });
        for url in [
            "https://bitbucket.example.com/scm/acme/sample-app.git",
            "https://jdoe@bitbucket.example.com/scm/acme/sample-app",
            "ssh://git@bitbucket.example.com:7999/acme/sample-app.git",
            "git@bitbucket.example.com:acme/sample-app.git",
        ] {
            assert_eq!(parse_bitbucket_origin(Some(url)), want, "{url}");
        }
        for url in ["https://bitbucket.example.com/scm/~jdoe/tools.git", "git@github.com:someone/tools.git", "https://git.example.com/some/mirror.git", ""] {
            assert_eq!(parse_bitbucket_origin(Some(url)), None, "{url}");
        }
        assert_eq!(parse_bitbucket_origin(None), None);
    }

    #[test]
    fn parse_github_origin_reads_https_scp_and_ssh_remotes_and_refuses_bitbucket_scm_paths() {
        let p = |u: &str| parse_github_origin(Some(u));
        let o = |a: &str, b: &str| Some(ParsedOrigin { project: a.into(), repo: b.into(), key: format!("{a}/{b}") });
        assert_eq!(p("https://github.com/Octo/hello.git"), o("Octo", "hello"));
        assert_eq!(p("https://github.com/Octo/hello"), o("Octo", "hello"));
        assert_eq!(p("https://token@github.com/octo/hello.git/"), o("octo", "hello"));
        assert_eq!(p("git@github.com:octo/.github.git").unwrap().repo, ".github");
        assert_eq!(p("ssh://git@ghe.example.com:2222/octo/hello.git"), o("octo", "hello"));
        for bad in ["https://bitbucket.example.com/scm/acme/sample-app.git", "https://github.com/only-one-part", ""] {
            assert_eq!(p(bad), None, "{bad}");
        }
        assert_eq!(parse_github_origin(None), None);
    }

    #[test]
    fn discover_local_clones_finds_clones_one_level_deep_skipping_worktrees_and_non_repos() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        make_fake_clone(&r.join("sample-app"), "https://bitbucket.example.com/scm/acme/sample-app.git");
        make_fake_clone(&r.join("tools"), "git@github.com:someone/tools.git");
        fs::create_dir(r.join("plain-folder")).unwrap();
        fs::create_dir_all(r.join("sample-app.worktrees/pr-review")).unwrap();
        fs::create_dir(r.join("linked")).unwrap();
        fs::write(r.join("linked/.git"), "gitdir: /elsewhere\n").unwrap();
        let roots = [r.to_path_buf(), r.join("does-not-exist")];
        assert_eq!(discover_local_clones(&roots, GitRemoteRules::BitbucketDc), [LocalClone { key: "ACME/sample-app".into(), path: s(&r.join("sample-app")) }]);
        assert_eq!(discover_local_clones(&roots, GitRemoteRules::Github), [LocalClone { key: "someone/tools".into(), path: s(&r.join("tools")) }]);
        // Listing the same root twice finds each clone once.
        assert_eq!(discover_local_clones(&[r.to_path_buf(), r.to_path_buf()], GitRemoteRules::Github).len(), 1);
    }

    #[test]
    fn the_origin_is_read_straight_from_git_config_even_with_crlf_and_other_sections_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let cfg = |text: &str| {
            fs::write(dir.path().join(".git/config"), text).unwrap();
            read_origin_from_git_config(dir.path())
        };
        assert_eq!(cfg("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = https://x/y.git\n\tfetch = +refs/*\n[branch \"m\"]\n\tremote = origin\n").as_deref(), Some("https://x/y.git"));
        assert_eq!(cfg("[remote \"origin\"]\r\n\turl = https://x/crlf.git\r\n").as_deref(), Some("https://x/crlf.git"));
        assert_eq!(cfg("[remote \"upstream\"]\n\turl = https://x/up.git\n"), None);
        assert_eq!(cfg("[remote \"origin\"]\n\tfetch = +refs/*\n"), None);
        assert_eq!(cfg("[remote   \"origin\" ]\n   url   =   https://x/spaced.git  \n").as_deref(), Some("https://x/spaced.git"));
        assert_eq!(read_origin_from_git_config(Path::new("/no/such")), None);
    }

    #[test]
    fn is_safe_repo_segment_accepts_names_and_rejects_path_tricks() {
        for ok in ["ACME", "sample-app", "libs.commons_2", ".github", ".hidden"] {
            assert!(is_safe_repo_segment(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a..b", "../etc", "a/b", "-rf", ".-x", ".git", ".GIT", "with space"] {
            assert!(!is_safe_repo_segment(bad), "{bad}");
        }
        assert!(!is_safe_repo_segment_value(Some(&json!(42))) && !is_safe_repo_segment_value(None));
    }

    #[test]
    fn derive_clone_url_reuses_the_transport_of_an_existing_clones_origin() {
        let derive = |origin: &str| derive_clone_url(&[origin], None, "ACME", "sample-service", GitRemoteRules::BitbucketDc);
        assert_eq!(derive("https://bitbucket.example.com/scm/acme/sample-app.git").as_deref(), Some("https://bitbucket.example.com/scm/acme/sample-service.git"));
        assert_eq!(derive("ssh://git@bitbucket.example.com:7999/other/sample-app.git").as_deref(), Some("ssh://git@bitbucket.example.com:7999/acme/sample-service.git"));
    }

    #[test]
    fn derive_clone_url_skips_other_providers_origins_and_falls_back_to_the_page_origin() {
        assert_eq!(
            derive_clone_url(&["https://github.com/some/mirror.git"], Some("https://bitbucket.example.com/"), "ACME", "sample-app", GitRemoteRules::BitbucketDc).as_deref(),
            Some("https://bitbucket.example.com/scm/acme/sample-app.git")
        );
        assert_eq!(derive_clone_url(&["git@github.com:some/mirror.git"], None, "Octo", "hello", GitRemoteRules::Github).as_deref(), Some("git@github.com:Octo/hello.git"));
        assert_eq!(derive_clone_url::<&str>(&[], None, "ACME", "sample-app", GitRemoteRules::Github), None);
        // `$` in the tail is literal, not a replacement reference.
        assert_eq!(derive_clone_url(&["git@github.com:a/b.git"], None, "o$1", "r", GitRemoteRules::Github).as_deref(), Some("git@github.com:o$1/r.git"));
    }

    #[test]
    fn clone_root_picks_the_folder_most_existing_clones_live_in() {
        let repos: Vec<PathBuf> = ["/x/gitviews/a", "/x/gitviews/b", "/y/other/c"].iter().map(PathBuf::from).collect();
        let home = Path::new("/home/me");
        assert_eq!(clone_root(&repos, &[PathBuf::from("/fallback")], home), PathBuf::from("/x/gitviews"));
        assert_eq!(clone_root(&[], &[PathBuf::from("/fallback")], home), PathBuf::from("/fallback"));
        assert_eq!(clone_root(&[], &[], home), PathBuf::from("/home/me/gitviews"));
        let tie: Vec<PathBuf> = ["/first/a", "/second/b"].iter().map(PathBuf::from).collect();
        assert_eq!(clone_root(&tie, &[], home), PathBuf::from("/first"), "ties go to the folder seen first");
    }

    #[test]
    fn resolve_review_editor_keeps_claude_code_and_cursor_and_ignores_a_saved_vscode() {
        assert_eq!(resolve_review_editor(Some("cursor")), Some("cursor"));
        assert_eq!(resolve_review_editor(Some("claude-code")), Some("claude-code"));
        assert_ne!(resolve_review_editor(Some("vscode")), Some("vscode"));
        assert!(detect_installed_editors().iter().all(|e| e.id != "vscode"));
        assert!(run_check_by_name("git").is_some_and(|c| c.ok) && run_check_by_name("nope").is_none());
    }

    #[test]
    fn each_providers_rules_keys_paths_urls_and_clone_sources() {
        use GitRemoteRules::{BitbucketDc as B, Github as G};
        assert_eq!(B.pr_key("acme", "Sample-App", "7"), "bitbucket:ACME/sample-app#7");
        assert_eq!(G.pr_key("Octo", "Hello", "7"), "github:octo/hello#7");
        assert_eq!((B.key_prefix(), G.key_prefix()), ("bitbucket", "github"));
        assert_eq!(B.pr_path("ACME", "sample-app", 7), "/projects/ACME/repos/sample-app/pull-requests/7");
        assert_eq!(G.pr_path("Octo", "hello", 7), "/Octo/hello/pull/7");
        assert_eq!(G.pr_path("a b", "c", 1), "/a%20b/c/pull/1");
        let pr = |p: &str, r: &str, id: u64| Some(PrRef { project: p.into(), repo: r.into(), id });
        assert_eq!(B.parse_pr_url("https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/7/overview"), pr("ACME", "sample-app", 7));
        assert_eq!(G.parse_pr_url("https://github.com/Octo/hello/pull/7/files"), pr("Octo", "hello", 7));
        assert_eq!(G.parse_pr_url("/Octo/hello/pull/7"), pr("Octo", "hello", 7));
        assert_eq!(G.parse_pr_url("https://ghe.example.com/api/v3/Octo/hello/pull/7"), pr("Octo", "hello", 7), "the API prefix is looked through");
        assert_eq!(G.parse_pr_url("https://github.com/Octo/hello/issues/7"), None);
        assert_eq!(B.parse_pr_url("https://github.com/Octo/hello/pull/7"), None);
        let repo = |p: &str, r: &str| Some(RepoRef { project: p.into(), repo: r.into() });
        assert_eq!(G.parse_repo_url("https://github.com/Octo/hello/tree/main"), repo("Octo", "hello"));
        assert_eq!(B.parse_repo_url("https://bb.example.com/projects/ACME/repos/sample-app/browse"), repo("ACME", "sample-app"));
        assert_eq!(G.parse_repo_url("https://github.com/"), None);
        assert_eq!(B.page_clone_url("https://bb.example.com/", "ACME", "sample-app"), "https://bb.example.com/scm/acme/sample-app.git");
        assert_eq!(G.page_clone_url("https://github.com/", "Octo", "hello"), "https://github.com/Octo/hello.git");
        assert_eq!(G.page_clone_url("https://ghe.example.com", "o", ".github"), "https://ghe.example.com/o/.github.git");
    }

    #[test]
    fn the_rules_in_force_are_the_profiles_git_providers_and_default_to_bitbucket() {
        // The framework placeholder's git site is GitHub.
        assert_eq!(git_remote_rules(), GitRemoteRules::Github);
        assert_eq!(GitRemoteRules::for_provider("bitbucket-dc"), Some(GitRemoteRules::BitbucketDc));
        assert_eq!(GitRemoteRules::for_provider("gitlab"), None);
        assert_eq!(default_search_roots(Path::new("/definitely/not/a/home")), Vec::<PathBuf>::new());
    }

    fn golden() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/prereqs/golden.json")).unwrap()
    }

    fn parsed(p: Option<ParsedOrigin>) -> Value {
        serde_json::to_value(p).unwrap()
    }

    /// The Node implementation's answers over a broad table of origin URLs, page
    /// URLs and `.git/config` texts, compared exactly: the regexes are the risky
    /// part of this port.
    #[test]
    fn answers_match_what_the_node_implementation_gave_over_a_broad_table() {
        let g = golden();
        for (origin, want) in g["origins"].as_object().unwrap() {
            assert_eq!(parsed(parse_bitbucket_origin(Some(origin))), want["bb"], "bitbucket origin {origin:?}");
            assert_eq!(parsed(parse_github_origin(Some(origin))), want["gh"], "github origin {origin:?}");
        }
        for m in g["matches"].as_array().unwrap() {
            let got = origin_matches_project_repo(m["o"].as_str(), m["p"].as_str().unwrap(), m["r"].as_str().unwrap());
            assert_eq!(got, m["m"].as_bool().unwrap(), "matches {m}");
        }
        for (url, by_provider) in g["urls"].as_object().unwrap() {
            for (provider, want) in by_provider.as_object().unwrap() {
                let rules = GitRemoteRules::for_provider(provider).unwrap();
                assert_eq!(serde_json::to_value(rules.parse_pr_url(url)).unwrap(), want["pr"], "{provider} pr {url:?}");
                assert_eq!(serde_json::to_value(rules.parse_repo_url(url)).unwrap(), want["repo"], "{provider} repo {url:?}");
            }
        }
        for c in g["clone"].as_array().unwrap() {
            let rules = GitRemoteRules::for_provider(c["k"].as_str().unwrap()).unwrap();
            let origin = c["o"].as_str().unwrap();
            let got = derive_clone_url(&[origin], c["page"].as_str(), "ACME", "svc", rules);
            assert_eq!(serde_json::to_value(got).unwrap(), c["v"], "clone {c}");
        }
        for c in g["configs"].as_array().unwrap() {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join(".git")).unwrap();
            fs::write(dir.path().join(".git/config"), c["text"].as_str().unwrap()).unwrap();
            assert_eq!(serde_json::to_value(read_origin_from_git_config(dir.path())).unwrap(), c["origin"], "config {}", c["text"]);
        }
    }
}
