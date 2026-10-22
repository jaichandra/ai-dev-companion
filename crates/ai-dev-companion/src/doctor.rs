//! `companion doctor` (port of `companion-service/doctor.js`): checks every piece the extension
//! depends on, in the order they depend on each other, and says exactly what to do about the first
//! thing that's wrong. Read-only: never changes config or restarts anything (the one thing it writes
//! is `bitbucket-version.json` under the `--bitbucket-contract` run).
//!
//! Everything that touches the machine or the network goes through [`DoctorIo`] (and
//! [`BitbucketProbe`] for the live contract check), so the lines can be tested with fakes. Output is
//! collected in a [`Report`] (`lines` are what `console.log` printed, `fixes` what the closing
//! "To fix, start with" list holds); [`finish`] adds the epilogue and gives the exit code.
//!
//! Divergences from Node: no Node-version check and no `dist/` build checks (there is nothing to
//! build: the read-only guard is a subcommand of the binary itself); the pack-derived values
//! (feature registry, doctor checks, manifest check, external-token sources) are handed in through
//! [`DoctorCtx`] instead of read from globals.

use crate::atlassian::AuthContext;
use crate::bitbucket_contract::{compare_version, missing_keys, missing_keys_across, parse_pr_url};
use crate::bitbucket_normalize::{normalize_app_properties, EXPECTED_KEYS, OPTIONAL_PULL_REQUEST_KEYS};
use crate::cli_format::one_line_str;
use crate::credential_store::CredentialStore;
use crate::feature_registry::FeatureRegistry;
use crate::js_text::slice_utf16;
use crate::launchd;
use crate::mcp_registration::registration_status;
use crate::packs::DoctorCheck;
use crate::plugin_info::{claude_config_dir, mcp_path_summary, plugin_status, Level};
use crate::prereqs::Check;
use crate::schedule::{budget_limit, digest_settings, quiet_hours_from, watcher_settings};
use crate::service_control::{tail_file, Health, HealthState};
use crate::similar::similar_enabled;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// What `console.log` printed and the fixes `report` collected.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<String>,
    pub fixes: Vec<String>,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    /// `OK   - message`; a fix is kept for the closing list unless the level is OK.
    pub fn report(&mut self, level: &str, message: &str, fix: Option<&str>) {
        self.lines.push(format!("{level:<4} - {message}"));
        if let (Some(fix), true) = (fix, level != "OK") {
            self.fixes.push(fix.to_string());
        }
    }

    /// A plain `console.log` line.
    pub fn say(&mut self, text: &str) {
        self.lines.push(text.to_string());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// Where the files doctor looks at live.
#[derive(Debug, Clone)]
pub struct DoctorPaths {
    /// The folder `config.json` lives in (and the one to `cd` to in a fix).
    pub companion_dir: PathBuf,
    pub config_path: PathBuf,
    pub credentials_path: PathBuf,
    pub credentials_key_path: PathBuf,
    pub state_dir: PathBuf,
    pub home: PathBuf,
    /// The generated `companion-config.js` the extension loads.
    pub extension_config_path: PathBuf,
    /// `$CLAUDE_CONFIG_DIR`.
    pub claude_config_dir_env: Option<String>,
    /// `$PATH`.
    pub path_env: Option<String>,
}

impl DoctorPaths {
    fn claude_json(&self) -> PathBuf {
        self.home.join(".claude.json")
    }
    fn cursor_dir(&self) -> PathBuf {
        self.home.join(".cursor")
    }
}

pub use crate::llm::LlmView;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskView {
    Ok { tests: usize, generated_at: Option<String>, stale: bool },
    Err(String),
}

/// What doctor does to the machine and the network.
#[async_trait]
pub trait DoctorIo: Send + Sync {
    /// Whether the `claude` CLI is installed.
    fn claude_cli_ok(&self) -> bool;
    /// A prerequisite check by its descriptor name (`git`, `claudeCli`, `claudeAuth`).
    fn run_check(&self, name: &str) -> Option<Check>;
    fn check_repo_path(&self, path: &str) -> Check;
    /// `git -C <repo> config --local --get hook.companion-precheck.command` succeeds.
    fn git_hook_configured(&self, repo_path: &str) -> bool;
    /// `git ls-remote --exit-code --heads origin` in the repo (no password prompt): its exit code,
    /// or `None` when git couldn't run or timed out.
    fn git_ls_remote_status(&self, repo_path: &str) -> Option<i32>;
    /// The LLM backend's state (the backend decides what a check costs), or `None` when there is
    /// no backend; an error is a surprise.
    async fn llm(&self, config: &Value) -> Result<Option<LlmView>, String>;
    async fn risk_facts(&self, url: &str) -> RiskView;
    async fn health(&self, port: u16, secret: &str) -> Health;
    fn launchd_installed(&self) -> bool;
    fn launchd_status(&self) -> launchd::Status;
    fn launchd_log_paths(&self) -> (PathBuf, PathBuf);
    /// Whether `path` is an executable file.
    fn is_executable(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            path.is_file()
        }
    }
}

/// Live, read-only access to Bitbucket for `--bitbucket-contract`.
#[async_trait]
pub trait BitbucketProbe: Send + Sync {
    fn base_url(&self) -> String;
    /// The credential the live check needs a saved value of.
    fn token_name(&self) -> String {
        "bitbucket.apiToken".into()
    }
    async fn application_properties_raw(&self, auth: &AuthContext) -> Result<Value, String>;
    async fn pull_request_raw(&self, auth: &AuthContext, project: &str, repo: &str, id: i64) -> Result<Value, String>;
    /// All activity pages and whether the page cap was hit first.
    async fn activity_pages(&self, auth: &AuthContext, project: &str, repo: &str, id: i64) -> Result<(Vec<Value>, bool), String>;
}

/// Site id -> where its token was borrowed from, for the base URLs given.
pub type ExternalSourcesFn = dyn Fn(&Map<String, Value>) -> Map<String, Value> + Send + Sync;

/// The pack- and profile-derived values doctor needs, handed in rather than read from globals.
pub struct DoctorCtx<'a> {
    pub paths: DoctorPaths,
    pub platform: Platform,
    pub io: &'a dyn DoctorIo,
    pub registry: &'a FeatureRegistry,
    pub product_name: String,
    /// The packs' doctor checks (`{config}` in, a line or nothing out).
    pub pack_checks: Vec<DoctorCheck>,
    /// The tool the packs borrow tokens from ("some-tool").
    pub external_label: String,
    /// Site id -> where its token was borrowed from, for the base URLs given.
    pub external_sources: &'a ExternalSourcesFn,
    /// The sites whose tokens live in the credential store (`jira`, `jenkins`, `bitbucket`), with
    /// each one's default base URL.
    pub sites: Vec<(String, String)>,
    /// Problems with the extension's `manifest.json` for this config (empty when it matches).
    pub manifest_problems: &'a (dyn Fn(&Value) -> Vec<String> + Send + Sync),
    /// How many protected-path rules every Claude run gets.
    pub protected_rule_count: usize,
    pub now_ms: &'a (dyn Fn() -> i64 + Send + Sync),
}

impl DoctorCtx<'_> {
    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.paths.credentials_path, &self.paths.credentials_key_path)
    }

    fn rerun_setup(&self) -> String {
        "companion setup".to_string()
    }
}

/// Whether `bin` is an executable file somewhere on `PATH`.
pub fn is_on_path(bin: &str, path_env: Option<&str>, io: &dyn DoctorIo) -> bool {
    let sep = if cfg!(windows) { ';' } else { ':' };
    path_env.unwrap_or("").split(sep).any(|dir| !dir.is_empty() && io.is_executable(&Path::new(dir).join(bin)))
}

/// Whether `p` is owner-only (mode 0600); `None` when it doesn't exist.
fn is_owner_only(p: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).ok().map(|m| m.permissions().mode() & 0o777 == 0o600)
    }
    #[cfg(not(unix))]
    {
        p.exists().then_some(true)
    }
}

/// One line, at most 200 characters: safe to print from untrusted text.
pub fn safe_text(value: &str) -> String {
    slice_utf16(&one_line_str(value), 200)
}

fn nonempty_str(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.trim().is_empty())
}

/// Encrypted credential store: can it be decrypted, are its two files owner-only, and has every
/// apiToken finished moving out of config.json. A missing credentials.enc is OK ("none saved yet").
pub fn check_credentials(r: &mut Report, ctx: &DoctorCtx<'_>, config: &Value) {
    let store = ctx.store();
    let names = match store.list_names() {
        Ok(n) => Some(n),
        Err(e) => {
            r.report("FAIL", &format!("Encrypted credentials: {e}"), Some(&e.to_string()));
            None
        }
    };
    if let Some(names) = &names {
        r.report(
            "OK",
            &if names.is_empty() { "Encrypted credentials: none saved yet.".to_string() } else { format!("Encrypted credentials: {} token(s) saved, decrypts fine ({}).", names.len(), ctx.paths.credentials_path.display()) },
            None,
        );
    }
    for (label, file) in [("credentials.enc", &ctx.paths.credentials_path), ("credentials.key", &ctx.paths.credentials_key_path)] {
        let Some(owner_only) = is_owner_only(file) else { continue };
        r.report(
            if owner_only { "OK" } else { "WARN" },
            &format!("{label} permissions: {}", if owner_only { "owner-only (0600)." } else { "looser than 0600 \u{2014} should be owner-only." }),
            Some(&format!("chmod 600 {}", file.display())),
        );
    }

    // Tokens borrowed from another tool (a pack's externalTokens) when none is saved here.
    let urls: Map<String, Value> = ctx.sites.iter().map(|(site, default)| (site.clone(), json!(nonempty_str(&config[site.as_str()]["baseUrl"]).map(String::from).unwrap_or_else(|| default.clone())))).collect();
    let saved = names.unwrap_or_default();
    let used: Vec<String> = (ctx.external_sources)(&urls).iter().filter(|(site, _)| !saved.contains(&format!("{site}.apiToken"))).map(|(site, src)| format!("{site} <- {}", src.as_str().unwrap_or(""))).collect();
    if !used.is_empty() {
        r.report("OK", &format!("Tokens from {} (none saved here): {}.", ctx.external_label, used.join("; ")), None);
    }

    let left: Vec<&str> = ctx.sites.iter().map(|(s, _)| s.as_str()).filter(|s| nonempty_str(&config[*s]["apiToken"]).is_some()).collect();
    if left.is_empty() {
        r.report("OK", "No API tokens left in config.json.", None);
    } else {
        r.report(
            "FAIL",
            &format!("config.json still has apiToken set for: {} \u{2014} this should have moved to the encrypted credential store automatically.", left.join(", ")),
            Some("Restart the companion service (it migrates any remaining token at startup), or re-run `companion setup`."),
        );
    }
}

fn hm(minutes: i32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Background work: which watchers are on, the Claude budget, quiet hours, the digest, the LLM,
/// the similar-tickets search and the pre-push hook. A surprise becomes one WARN line.
pub async fn check_background(r: &mut Report, ctx: &DoctorCtx<'_>, config: &Value) {
    let mut lines = Report::new();
    match check_background_lines(&mut lines, ctx, config).await {
        Ok(()) => {
            r.lines.extend(lines.lines);
            r.fixes.extend(lines.fixes);
        }
        Err(e) => r.report("WARN", &format!("Background work: couldn't be checked ({}).", slice_utf16(&e, 160)), None),
    }
}

async fn check_background_lines(r: &mut Report, ctx: &DoctorCtx<'_>, config: &Value) -> Result<(), String> {
    let watchers: Vec<_> = watcher_settings(config).into_iter().filter(|(_, w)| w.enabled).collect();
    r.report(
        "OK",
        &if watchers.is_empty() {
            "Background watchers: all off (optional \u{2014} turn them on in \u{2699} Settings \u{2192} Background work).".to_string()
        } else {
            format!("Background watchers: {}; at most {} Claude run(s) a day.", watchers.iter().map(|(n, w)| format!("{n} every {} min", w.interval_minutes)).collect::<Vec<_>>().join(", "), budget_limit(config))
        },
        None,
    );
    if let Some(q) = quiet_hours_from(config) {
        r.report("OK", &format!("Quiet hours: {}\u{2013}{} (no watcher runs, notifications held for the digest).", hm(q.start), hm(q.end)), None);
    }
    let digest = digest_settings(config);
    r.report("OK", &if digest.enabled { format!("Morning digest: weekdays at {}.", digest.time) } else { "Morning digest: not scheduled (the \u{2728} Morning digest row still works).".to_string() }, None);

    match ctx.io.llm(config).await? {
        Some(llm) => {
            r.report(if llm.ok { "OK" } else { "WARN" }, &llm.line, Some(&llm.fix));
            let how = if llm.state == "ready" { llm.similar_when_ready.clone() } else { llm.similar_when_not_ready.clone() };
            check_similar(r, ctx, config, &how, &llm.embedding_model);
        }
        None => check_similar(r, ctx, config, "by shared words only", ""),
    }

    let repos: Vec<(&String, &str)> = config["repos"].as_object().map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|p| (k, p))).take(5).collect()).unwrap_or_default();
    let with_hook: Vec<&str> = repos.iter().filter(|(_, p)| ctx.io.git_hook_configured(p)).map(|(k, _)| k.as_str()).collect();
    if with_hook.is_empty() {
        r.report("OK", "Pre-push check: not installed in any repo (optional \u{2014} `companion hooks install` in a repo).", None);
    } else if is_on_path("companion", ctx.paths.path_env.as_deref(), ctx.io) {
        r.report("OK", &format!("Pre-push check installed in: {}.", with_hook.join(", ")), None);
    } else {
        r.report(
            "WARN",
            &format!("Pre-push check installed in {}, but `companion` isn't on your PATH, so git can't run it.", with_hook.join(", ")),
            Some("Re-run the installer (`install --yes`; it links `companion` into ~/.local/bin) and make sure ~/.local/bin is on your PATH."),
        );
    }
    Ok(())
}

/// Whether the companion's own MCP server is registered with Claude Code and, if installed, Cursor,
/// at the port and `mcp.token` this run uses. Never prints the token.
pub fn check_mcp_registration(r: &mut Report, ctx: &DoctorCtx<'_>, config: &Value) {
    let token = ctx.store().get_value_text("mcp.token").ok().flatten().filter(|t| !t.is_empty());
    let Some(token) = token else {
        r.report("WARN", "MCP token not created yet.", Some("re-run the installer (`install --yes`) or `companion setup`"));
        return;
    };
    let claude_available = ctx.io.claude_cli_ok();
    if !claude_available {
        r.report("OK", "Claude Code not installed \u{2014} MCP registration skipped.", None);
    }
    let read = |p: PathBuf| std::fs::read_to_string(p).ok();
    let cursor_installed = ctx.paths.cursor_dir().exists();
    let claude_text = read(ctx.paths.claude_json());
    let cursor_text = if cursor_installed { read(ctx.paths.cursor_dir().join("mcp.json")) } else { None };
    let port = config["port"].as_u64().unwrap_or(0) as u16;
    let statuses = registration_status(claude_text.as_deref(), cursor_text.as_deref(), cursor_installed, port, &token);
    for (client, state) in &statuses {
        if *client == "Claude Code" && !claude_available {
            continue;
        }
        let level = match *state {
            "ok" => "OK",
            "stale-token" | "wrong-url" => "FAIL",
            _ => "WARN",
        };
        let detail = match *state {
            "ok" => "registered and up to date",
            "missing" => "not registered",
            "stale-token" => "token doesn't match (it changes when you rotate it)",
            "wrong-url" => "registered at a different URL/port",
            _ => "its config file isn't valid JSON",
        };
        r.report(level, &format!("MCP registration ({client}): {detail}."), Some("companion setup"));
    }
    if claude_available {
        let state = statuses.iter().find(|(c, _)| *c == "Claude Code").map(|(_, s)| *s);
        check_plugin_path(r, ctx, state);
    }
}

/// Which way Claude Code reaches the companion: `claude mcp add`, the ai-companion plugin, or both.
/// Reads Claude Code's own plugin files; silent when neither mentions the plugin.
pub fn check_plugin_path(r: &mut Report, ctx: &DoctorCtx<'_>, claude_state: Option<&str>) {
    let dir = claude_config_dir(ctx.paths.claude_config_dir_env.as_deref(), &ctx.paths.home);
    let installed = std::fs::read_to_string(dir.join("plugins").join("installed_plugins.json")).ok();
    let settings = std::fs::read_to_string(dir.join("settings.json")).ok();
    let plugin = plugin_status(installed.as_deref(), settings.as_deref());
    if let Some(line) = mcp_path_summary(claude_state, Some(&plugin)) {
        r.report(if line.level == Level::Ok { "OK" } else { "WARN" }, &line.message, line.fix);
    }
}

/// Similar past tickets: on or off, and how many history items have an embedding. Opens history.db
/// read-only and only if it exists: doctor never creates or migrates it.
pub fn check_similar(r: &mut Report, ctx: &DoctorCtx<'_>, config: &Value, how: &str, embedding_model: &str) {
    if !similar_enabled(config) {
        r.report("OK", "Similar past tickets: off (\u{2699} Settings \u{2192} Local history).", None);
        return;
    }
    let file = ctx.paths.state_dir.join("history.db");
    if !file.exists() {
        r.report("OK", &format!("Similar past tickets: on, {how}; no local history on this machine yet (the installed copy keeps it)."), None);
        return;
    }
    match count_embedded(&file, embedding_model) {
        Ok((vectors, items)) => r.report("OK", &format!("Similar past tickets: on, {how}; {vectors} of {items} ticket, analysis and PR item(s) embedded."), None),
        Err(e) if e.to_lowercase().contains("no such table") => r.report("OK", &format!("Similar past tickets: on, {how}; the history is in an older layout (the service adds the vectors at its next start)."), None),
        Err(e) => r.report("WARN", &format!("Similar past tickets: on, {how}; the history file couldn't be read ({}). It may be corrupt or locked.", safe_text(&e)), None),
    }
}

/// Real vectors only: a one-number placeholder marks an item that could not be embedded.
fn count_embedded(file: &Path, model: &str) -> Result<(i64, i64), String> {
    use rusqlite::{Connection, OpenFlags};
    let db = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| e.to_string())?;
    let vectors: i64 = db.query_row("SELECT COUNT(*) FROM item_vectors WHERE dim > 1 AND model = ?", [model], |row| row.get(0)).map_err(|e| e.to_string())?;
    let items: i64 = db.query_row("SELECT COUNT(*) FROM items WHERE kind IN ('ticket', 'analysis', 'pr')", [], |row| row.get(0)).map_err(|e| e.to_string())?;
    Ok((vectors, items))
}

/// The extension's generated `companion-config.js`: the object assigned to `self.COMPANION_CONFIG`.
pub fn read_extension_config(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let rest = &text[text.find("COMPANION_CONFIG")? + "COMPANION_CONFIG".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim();
    let json = rest.trim_end().strip_suffix(';').unwrap_or(rest).trim();
    serde_json::from_str::<Value>(json).ok().filter(Value::is_object)
}

/// `doctor --bitbucket-contract [PR url]`. Read-only: only GET requests; the one thing written is
/// `<state dir>/bitbucket-version.json`. Without a PR url only `application-properties` (and so the
/// version) is checked.
pub async fn run_bitbucket_contract_check(r: &mut Report, ctx: &DoctorCtx<'_>, probe: &dyn BitbucketProbe, pr_url: Option<&str>) {
    r.say("== Bitbucket contract check ==\n");
    let token = ctx.store().get_value_text(&probe.token_name()).ok().flatten().filter(|t| !t.is_empty());
    if token.is_none() {
        r.say("skipped \u{2014} add a Bitbucket token in Settings to run the live contract check");
        return;
    }
    // No relayed browser session is available to a CLI tool: this always uses the saved token.
    let auth = AuthContext::default();

    let app_raw = match probe.application_properties_raw(&auth).await {
        Ok(v) => v,
        Err(e) => {
            r.report("FAIL", &format!("Could not reach Bitbucket at {}: {e}", probe.base_url()), None);
            return;
        }
    };
    let live_version = normalize_app_properties(&app_raw).and_then(|p| p.version);
    r.report(if live_version.is_some() { "OK" } else { "WARN" }, &format!("Bitbucket version: {}.", live_version.as_deref().unwrap_or("unknown")), None);
    let missing = missing_keys(&app_raw, EXPECTED_KEYS.application_properties);
    r.report(if missing.is_empty() { "OK" } else { "WARN" }, &format!("application-properties fields: {}", if missing.is_empty() { "all present.".to_string() } else { format!("missing {}.", missing.join(", ")) }), None);

    let version_file = ctx.paths.state_dir.join("bitbucket-version.json");
    let recorded: Option<Value> = std::fs::read_to_string(&version_file).ok().and_then(|t| serde_json::from_str(&t).ok());
    if let Some(live) = &live_version {
        if let Some(message) = compare_version(recorded.as_ref(), live).message {
            r.say(&message);
        }
        let seen_at = crate::js_date::to_iso_string((ctx.now_ms)()).unwrap_or_default();
        let body = format!("{}\n", serde_json::to_string_pretty(&json!({ "version": live, "seenAt": seen_at })).expect("a JSON value serializes"));
        let wrote = std::fs::create_dir_all(&ctx.paths.state_dir).and_then(|()| std::fs::write(&version_file, body));
        if let Err(e) = wrote {
            r.report("WARN", &format!("Could not record the Bitbucket version to {}: {e}", version_file.display()), None);
        }
    }

    let Some(pr_url) = pr_url else {
        r.say("\n(No PR URL given \u{2014} only application-properties was checked. Pass one to also check the pullRequest, activity, comment and commentAnchor fields.)");
        return;
    };
    let Some(parsed) = parse_pr_url(pr_url) else {
        r.report("FAIL", &format!("\"{pr_url}\" doesn't look like a Bitbucket PR URL (expected .../projects/P/repos/R/pull-requests/N)."), None);
        return;
    };
    let id = parsed.id as i64;
    let pr = match probe.pull_request_raw(&auth, &parsed.project, &parsed.repo, id).await {
        Ok(v) => v,
        Err(e) => {
            r.report("FAIL", &format!("Could not fetch {}/{}#{}: {e}", parsed.project, parsed.repo, parsed.id), None);
            return;
        }
    };
    let describe = |label: &str, missing: &[String]| format!("{label}: {}", if missing.is_empty() { "all present.".to_string() } else { format!("missing {}.", missing.join(", ")) });
    let missing = missing_keys(&pr, EXPECTED_KEYS.pull_request);
    r.report(if missing.is_empty() { "OK" } else { "WARN" }, &describe("pullRequest fields", &missing), None);
    // Optional keys are informational, never a WARN: a real 9.4.16 doesn't return them on this endpoint.
    let missing_optional = missing_keys(&pr, OPTIONAL_PULL_REQUEST_KEYS);
    r.report(
        "OK",
        &if missing_optional.is_empty() {
            "pullRequest optional fields: all present.".to_string()
        } else {
            format!("pullRequest optional fields not returned by this Bitbucket version: {} (the Address review comments menu then shows on every PR page).", missing_optional.join(", "))
        },
        None,
    );

    let (pages, truncated) = match probe.activity_pages(&auth, &parsed.project, &parsed.repo, id).await {
        Ok(p) => p,
        Err(e) => {
            r.report("FAIL", &format!("Could not fetch activities for {}/{}#{}: {e}", parsed.project, parsed.repo, parsed.id), None);
            return;
        }
    };
    if truncated {
        r.report("WARN", "Hit the activities page cap before the last page \u{2014} some older comments may be missing from this check.", None);
    }
    let activities: Vec<Value> = pages.iter().flat_map(|p| p["values"].as_array().cloned().unwrap_or_default()).collect();
    let commented: Vec<Value> = activities.into_iter().filter(|a| a["action"] == "COMMENTED").collect();

    let missing = missing_keys_across(&commented, EXPECTED_KEYS.activity);
    let text = if commented.is_empty() { "activity fields: no COMMENTED activities found on this PR \u{2014} nothing to check.".to_string() } else { describe(&format!("activity fields ({} sampled)", commented.len()), &missing) };
    r.report(if missing.is_empty() { "OK" } else { "WARN" }, &text, None);

    let comments: Vec<Value> = commented.iter().map(|a| a["comment"].clone()).filter(Value::is_object).collect();
    let missing = missing_keys_across(&comments, EXPECTED_KEYS.comment);
    let text = if comments.is_empty() { "comment fields: no comments found on this PR \u{2014} nothing to check.".to_string() } else { describe(&format!("comment fields ({} sampled)", comments.len()), &missing) };
    r.report(if missing.is_empty() { "OK" } else { "WARN" }, &text, None);

    let anchors: Vec<Value> = commented.iter().map(|a| a["commentAnchor"].clone()).filter(Value::is_object).collect();
    let missing = missing_keys_across(&anchors, EXPECTED_KEYS.comment_anchor);
    let text = if anchors.is_empty() { "commentAnchor fields: no anchored comments found on this PR \u{2014} nothing to check.".to_string() } else { describe(&format!("commentAnchor fields ({} sampled)", anchors.len()), &missing) };
    r.report(if missing.is_empty() { "OK" } else { "WARN" }, &text, None);
}

/// The ordinary doctor run. `config` is the loaded `config.json` (`None` when missing or invalid).
pub async fn run(ctx: &DoctorCtx<'_>, config: Option<Value>) -> Report {
    let mut r = Report::new();
    r.say(&format!("== {} doctor ==\n", ctx.product_name));
    let rerun = ctx.rerun_setup();
    let Some(config) = config else {
        r.report("FAIL", &format!("{} is missing or invalid \u{2014} setup hasn't been completed.", ctx.paths.config_path.display()), Some(&rerun));
        return r;
    };
    r.report("OK", &format!("Settings found ({}).", ctx.paths.config_path.display()), None);

    check_credentials(&mut r, ctx, &config);
    check_mcp_registration(&mut r, ctx, &config);
    check_background(&mut r, ctx, &config).await;

    let secret = config["sharedSecret"].as_str().unwrap_or("");
    let port = config["port"].as_u64().unwrap_or(0) as u16;
    let expected_url = format!("http://127.0.0.1:{port}");
    match read_extension_config(&ctx.paths.extension_config_path) {
        None => r.report("FAIL", &format!("{} is missing \u{2014} the extension can't reach the service.", ctx.paths.extension_config_path.display()), Some(&rerun)),
        Some(ext) if ext["sharedSecret"].as_str() != Some(secret) || ext["serviceBaseUrl"].as_str() != Some(expected_url.as_str()) => {
            r.report("FAIL", "The extension's connection settings don't match config.json.", Some(&rerun));
        }
        Some(_) => r.report("OK", "Extension connection settings match config.json.", None),
    }

    let problems = (ctx.manifest_problems)(&config);
    if problems.is_empty() {
        r.report("OK", "Extension manifest matches your servers and features.", None);
    } else {
        r.report("FAIL", &format!("The extension's manifest.json doesn't match your settings: {}", problems.join(" ")), Some(&format!("{rerun} Then reload the extension at chrome://extensions.")));
    }

    let enabled = ctx.registry.enabled_feature_ids(&config);
    let enabled_refs: Vec<&str> = enabled.iter().map(String::as_str).collect();
    r.report(if enabled.is_empty() { "WARN" } else { "OK" }, &format!("Enabled features: {}.", if enabled.is_empty() { "none".to_string() } else { enabled.join(", ") }), Some(&rerun));
    for name in ctx.registry.required_checks_for(&enabled_refs) {
        let Some(result) = ctx.io.run_check(&name) else { continue };
        let needing: Vec<&str> = ctx.registry.features_requiring(&name, &enabled_refs).iter().map(|d| d.label.as_str()).collect();
        let message = if result.ok { result.message.clone() } else { format!("{} (needed by {})", result.message, needing.join(", ")) };
        r.report(if result.ok { "OK" } else { "FAIL" }, &message, Some(&result.message));
    }

    for check in &ctx.pack_checks {
        if let Some(line) = check(&config) {
            r.report(if line.ok { "OK" } else { "FAIL" }, &line.message, None);
        }
    }

    match config["riskFacts"]["url"].as_str().filter(|u| !u.is_empty()) {
        None => r.report("OK", "Shared test history: not configured (optional).", None),
        Some(url) => match ctx.io.risk_facts(url).await {
            RiskView::Ok { tests, generated_at, stale } => r.report(
                "OK",
                &format!("Shared test history: {tests} tests, generated {}{}.", generated_at.as_deref().unwrap_or("at an unknown time"), if stale { " (stale copy)" } else { "" }),
                None,
            ),
            RiskView::Err(reason) => r.report("WARN", &format!("Shared test history: {reason}"), Some("Check the URL in \u{2699} Settings \u{2192} Shared test history.")),
        },
    }

    if enabled.iter().any(|id| id == "ticket-to-pr") {
        let transition = nonempty_str(&config["ticketToPr"]["reviewTransitionName"]).unwrap_or("In Review");
        r.report("OK", &format!("Ticket to PR: on \u{2014} Create PR moves the ticket with its \"{transition}\" transition when it has one."), None);
        // Create PR pushes with the user's own git credentials: a push that would stop at a password
        // prompt fails instead, so check each repo's origin answers without one. First 5 repos only.
        for (key, path) in config["repos"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|p| (k, p))).take(5) {
            if !Path::new(path).join(".git").exists() {
                r.report("WARN", &format!("Ticket to PR: {key} ({path}) isn't a git clone, so Start fix can't use it."), Some("Fix the path in \u{2699} Settings \u{2192} Repositories."));
                continue;
            }
            // ls-remote --exit-code: 2 = reached origin but it has no branches.
            match ctx.io.git_ls_remote_status(path) {
                Some(0) => r.report("OK", &format!("Ticket to PR: {key} reaches origin without a password prompt."), None),
                Some(2) => r.report("WARN", &format!("Ticket to PR: {key}'s origin answered but has no branches yet."), Some("Push a first branch to origin before using Start fix on it.")),
                _ => r.report(
                    "WARN",
                    &format!("Ticket to PR: {key} can't reach origin without a prompt, so Create PR's push would fail."),
                    Some(&format!("Make `git -C {path} push --dry-run` work in a terminal without a prompt (SSH key or a credential helper).")),
                ),
            }
        }
    }

    if ctx.registry.needs_repos(&enabled_refs) {
        let repos: Vec<(&String, &str)> = config["repos"].as_object().map(|m| m.iter().map(|(k, v)| (k, v.as_str().unwrap_or(""))).collect()).unwrap_or_default();
        if repos.is_empty() {
            r.report("OK", "No repos set up yet \u{2014} each is found, picked or cloned from the browser on first use.", None);
        }
        for (key, path) in repos {
            let result = ctx.io.check_repo_path(path);
            r.report(if result.ok { "OK" } else { "WARN" }, &format!("{key}: {}", result.message), Some(&format!("{key} will be looked up again (or offered to pick/clone) the next time it's used.")));
        }
    }

    // The read-only guard is a subcommand of the binary itself (`<binary> guard ...`), so there is no
    // script file to go missing; the protected-path rule count is informational.
    r.report("OK", &format!("MCP read-only guard built in (`guard` subcommand); {} protected-path rule(s) applied to every Claude run.", ctx.protected_rule_count), None);

    // Every readOnly/worktreeWrite Claude run sets `sandbox.enabled` with `failIfUnavailable`: macOS
    // (Seatbelt) needs nothing extra; Linux needs `bwrap` and `socat` on PATH; Windows has no sandbox.
    match ctx.platform {
        Platform::Mac => r.report("OK", "Claude Bash sandbox: enforced (macOS Seatbelt).", None),
        Platform::Linux => {
            let missing: Vec<&str> = ["bwrap", "socat"].into_iter().filter(|b| !is_on_path(b, ctx.paths.path_env.as_deref(), ctx.io)).collect();
            if missing.is_empty() {
                r.report("OK", "Claude Bash sandbox: enforced (Linux bubblewrap + socat).", None);
            } else {
                r.report(
                    "FAIL",
                    &format!("Claude Bash sandbox: {} not found on PATH \u{2014} Claude runs that use Bash will fail.", missing.join(", ")),
                    Some("Install bubblewrap and socat (e.g. `apt install bubblewrap socat` or `dnf install bubblewrap socat`), then come back here."),
                );
            }
        }
        Platform::Windows => r.report("WARN", "Claude Bash sandbox: not supported \u{2014} Claude runs that need Bash will fail.", None),
        Platform::Other => {}
    }

    let mac = ctx.platform == Platform::Mac;
    let background = mac && ctx.io.launchd_installed();
    let health = ctx.io.health(port, secret).await;
    if background {
        match ctx.io.launchd_status().pid {
            Some(pid) => r.report("OK", &format!("Background service is running (pid {pid})."), None),
            None => r.report("FAIL", "Background service is installed but not running.", Some(&rerun)),
        }
    } else if health.state == HealthState::Ok {
        r.report("OK", "Running in the foreground (`companion serve`) \u{2014} it stops when that terminal closes.", None);
    } else {
        let fix = if mac { format!("{rerun}   (answer yes to running it in the background)") } else { "companion serve   (keep that terminal open)".to_string() };
        r.report("FAIL", "The service isn't running.", Some(&fix));
    }

    match health.state {
        HealthState::Ok => r.report("OK", &health.message, None),
        HealthState::Unauthorized => r.report("FAIL", &health.message, Some(&format!("Stop whatever else is on port {port} (`lsof -nP -iTCP:{port} -sTCP:LISTEN`), then: {rerun}"))),
        _ => {
            r.report("FAIL", &format!("{} The extension will say \"Could not reach the companion service\".", health.message), None);
            if background {
                let (out, err) = ctx.io.launchd_log_paths();
                let tail: Vec<String> = [tail_file(&out, 8), tail_file(&err, 8)].into_iter().filter(|t| !t.is_empty()).collect();
                if !tail.is_empty() {
                    let indented: Vec<String> = tail.join("\n").split('\n').map(|l| format!("     {l}")).collect();
                    r.say(&format!("\n     Last lines of the service log:\n{}\n", indented.join("\n")));
                }
            }
        }
    }
    r
}

/// The closing lines and the exit code. A contract run has already said everything worth saying, so
/// it gets no "reload the extension" epilogue.
pub fn finish(r: &mut Report, contract_run: bool) -> i32 {
    if contract_run {
        return i32::from(!r.fixes.is_empty());
    }
    if r.fixes.is_empty() {
        r.say("\nEverything looks good. If the \u{2728} button still misbehaves, reload the extension at\nchrome://extensions and refresh the page.");
        return 0;
    }
    r.say("\nTo fix, start with:");
    let mut seen: Vec<String> = vec![];
    for fix in r.fixes.clone() {
        if !seen.contains(&fix) {
            r.say(&format!("  \u{2022} {fix}"));
            seen.push(fix);
        }
    }
    r.say("\nAfter fixing, reload the extension at chrome://extensions and refresh the page.");
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::{DoctorLine, FeatureDescriptor};
    use std::sync::Mutex;

    struct FakeIo {
        claude: bool,
        llm: Result<Option<LlmView>, String>,
        health: Health,
        hooks: Vec<String>,
        ls_remote: Option<i32>,
        executables: Vec<PathBuf>,
        launchd_installed: bool,
        calls: Mutex<Vec<String>>,
    }

    fn llm(state: &str) -> LlmView {
        LlmView {
            state: state.into(),
            embedding_model: "embed-m".into(),
            ok: state == "ready" || state == "no-key",
            line: format!("Fake LLM: {state} (chat-m).{}", if state == "unreachable" { " Are you on the VPN?" } else { "" }),
            fix: "Check the key in the fake settings.".into(),
            similar_when_ready: "by meaning (embed-m) and shared words".into(),
            similar_when_not_ready: "by shared words only, until the fake LLM is ready".into(),
        }
    }

    fn health(state: HealthState, message: &str) -> Health {
        Health { state, message: message.into(), features: None }
    }

    fn io() -> FakeIo {
        FakeIo { claude: true, llm: Ok(Some(llm("no-key"))), health: health(HealthState::Ok, "The service answers."), hooks: vec![], ls_remote: Some(0), executables: vec![], launchd_installed: false, calls: Mutex::new(vec![]) }
    }

    #[async_trait]
    impl DoctorIo for FakeIo {
        fn claude_cli_ok(&self) -> bool {
            self.claude
        }
        fn run_check(&self, name: &str) -> Option<Check> {
            self.calls.lock().unwrap().push(format!("check {name}"));
            Some(Check { ok: name != "claudeAuth", message: format!("{name} check") })
        }
        fn check_repo_path(&self, path: &str) -> Check {
            Check { ok: path.starts_with("/ok"), message: format!("{path} is a repo") }
        }
        fn git_hook_configured(&self, repo_path: &str) -> bool {
            self.hooks.iter().any(|h| h == repo_path)
        }
        fn git_ls_remote_status(&self, _: &str) -> Option<i32> {
            self.ls_remote
        }
        async fn llm(&self, _: &Value) -> Result<Option<LlmView>, String> {
            self.llm.clone()
        }
        async fn risk_facts(&self, _: &str) -> RiskView {
            RiskView::Ok { tests: 3, generated_at: None, stale: true }
        }
        async fn health(&self, _: u16, _: &str) -> Health {
            self.health.clone()
        }
        fn launchd_installed(&self) -> bool {
            self.launchd_installed
        }
        fn launchd_status(&self) -> launchd::Status {
            launchd::Status { loaded: true, pid: Some("42".into()) }
        }
        fn launchd_log_paths(&self) -> (PathBuf, PathBuf) {
            (PathBuf::from("/nope/out"), PathBuf::from("/nope/err"))
        }
        fn is_executable(&self, path: &Path) -> bool {
            self.executables.iter().any(|e| e == path)
        }
    }

    struct Fx {
        _d: tempfile::TempDir,
        root: PathBuf,
        registry: FeatureRegistry,
    }

    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let mut a = FeatureDescriptor::new("review-in-editor", "Review PR");
        a.required_checks = vec!["claudeCli".into(), "claudeAuth".into()];
        a.needs_repos = true;
        let b = FeatureDescriptor::new("ticket-to-pr", "Ticket to PR");
        Fx { _d: d, root, registry: FeatureRegistry::new(vec![a, b]) }
    }

    fn ctx<'a>(f: &'a Fx, io: &'a FakeIo, platform: Platform, path_env: Option<&str>) -> DoctorCtx<'a> {
        DoctorCtx {
            paths: DoctorPaths {
                companion_dir: f.root.join("companion"),
                config_path: f.root.join("companion/config.json"),
                credentials_path: f.root.join("companion/credentials.enc"),
                credentials_key_path: f.root.join("state/credentials.key"),
                state_dir: f.root.join("state"),
                home: f.root.join("home"),
                extension_config_path: f.root.join("ext/companion-config.js"),
                claude_config_dir_env: None,
                path_env: path_env.map(String::from),
            },
            platform,
            io,
            registry: &f.registry,
            product_name: "Test Product".into(),
            pack_checks: vec![],
            external_label: "some-tool".into(),
            external_sources: &|_| Map::new(),
            sites: vec![("jira".into(), "https://jira.example".into()), ("bitbucket".into(), "https://bb.example".into())],
            manifest_problems: &|_| vec![],
            protected_rule_count: 7,
            now_ms: &|| 1_700_000_000_000,
        }
    }

    fn text(r: &Report) -> String {
        r.lines.join("\n")
    }

    fn all_lines_are_reports(r: &Report) -> bool {
        r.lines.iter().all(|l| l.starts_with("OK   - ") || l.starts_with("WARN - ") || l.starts_with("FAIL - "))
    }

    // doctor.test.js: an empty config reports the background work as off and never throws.
    #[tokio::test]
    async fn an_empty_config_reports_the_background_work_as_off_and_never_throws() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_background(&mut r, &c, &json!({})).await;
        let t = text(&r);
        assert!(t.contains("Background watchers: all off"), "{t}");
        assert!(t.contains("Morning digest: not scheduled"));
        assert!(t.contains("Fake LLM: "));
        assert!(t.contains("Pre-push check: not installed in any repo"));
        assert!(all_lines_are_reports(&r), "{t}");
    }

    #[tokio::test]
    async fn enabled_watchers_and_quiet_hours_are_shown_from_the_parsed_values_not_the_raw_strings() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_background(&mut r, &c, &json!({ "watchers": { "conflicts": { "enabled": true, "intervalMinutes": 15 } }, "scheduler": { "quietHours": { "start": "19:00", "end": "07:00" } }, "digest": { "enabled": true, "time": "08:30" } })).await;
        let t = text(&r);
        assert!(t.contains("Background watchers: conflicts every 15 min; at most 5 Claude run(s) a day"), "{t}");
        assert!(t.contains("Quiet hours: 19:00\u{2013}07:00"), "{t}");
        assert!(t.contains("Morning digest: weekdays at 08:30"));
        let mut odd = Report::new();
        check_background(&mut odd, &c, &json!({ "scheduler": { "quietHours": { "start": "\u{1b}[2J", "end": "07:00" } }, "watchers": 5 })).await;
        assert!(!text(&odd).contains('\u{1b}'));
    }

    #[tokio::test]
    async fn a_surprise_inside_the_check_becomes_one_warn_line_instead_of_stopping_doctor() {
        let f = fx();
        let mut io = io();
        io.llm = Err("bad config".into());
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_background(&mut r, &c, &json!({})).await;
        assert_eq!(r.lines, ["WARN - Background work: couldn't be checked (bad config)."]);
    }

    #[tokio::test]
    async fn the_llm_line_is_the_backends_and_only_its_ok_flag_decides_the_level() {
        let f = fx();
        let bare = FakeIo { llm: Ok(None), ..io() };
        let mut io = io();
        io.llm = Ok(Some(llm("unreachable")));
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_background(&mut r, &c, &json!({})).await;
        assert!(text(&r).contains("WARN - Fake LLM: unreachable (chat-m). Are you on the VPN?"), "{}", text(&r));
        assert!(r.fixes.iter().any(|x| x.contains("Check the key")));
        // Without a backend there is no LLM line, and similar items say words only.
        let c = ctx(&f, &bare, Platform::Mac, None);
        let mut none = Report::new();
        check_background(&mut none, &c, &json!({})).await;
        assert!(!text(&none).contains("Fake LLM") && text(&none).contains("Similar past tickets: on, by shared words only;"), "{}", text(&none));
    }

    #[tokio::test]
    async fn a_pre_push_hook_without_companion_on_path_is_a_warning_and_with_it_is_ok() {
        let f = fx();
        let mut io = io();
        io.hooks = vec!["/r/a".into()];
        let config = json!({ "repos": { "ACME/a": "/r/a", "ACME/b": "/r/b" } });
        let c = ctx(&f, &io, Platform::Mac, Some("/bin:/usr/local/bin"));
        let mut r = Report::new();
        check_background(&mut r, &c, &config).await;
        assert!(text(&r).contains("WARN - Pre-push check installed in ACME/a, but `companion` isn't on your PATH"));
        io.executables = vec![PathBuf::from("/usr/local/bin/companion")];
        let c = ctx(&f, &io, Platform::Mac, Some("/bin:/usr/local/bin"));
        let mut r = Report::new();
        check_background(&mut r, &c, &config).await;
        assert!(text(&r).contains("OK   - Pre-push check installed in: ACME/a."));
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn doctor_says_which_way_claude_code_reaches_the_companion() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let capture = |state: Option<&str>| {
            let mut r = Report::new();
            check_plugin_path(&mut r, &c, state);
            text(&r)
        };
        assert_eq!(capture(Some("ok")), "OK   - Claude Code reaches the companion through `claude mcp add` (user scope).");
        assert_eq!(capture(Some("missing")), "", "neither path: the registration line already says so");
        let dir = c.paths.home.join(".claude");
        write(&dir.join("plugins/installed_plugins.json"), &json!({ "version": 2, "plugins": { "ai-companion@ai-dev-companion": [{}] } }).to_string());
        write(&dir.join("settings.json"), &json!({ "enabledPlugins": { "ai-companion@ai-dev-companion": true } }).to_string());
        assert!(capture(Some("ok")).starts_with("WARN - Claude Code reaches the companion both through `claude mcp add` and the ai-companion plugin"));
        assert!(capture(Some("missing")).starts_with("OK   - Claude Code reaches the companion through the ai-companion plugin."));
    }

    #[test]
    fn check_plugin_path_tolerates_malformed_and_missing_files_and_reports_installed_but_disabled() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let capture = |state: &str| {
            let mut r = Report::new();
            check_plugin_path(&mut r, &c, Some(state));
            text(&r)
        };
        let dir = c.paths.home.join(".claude");
        assert_eq!(capture("missing"), "", "no files at all");
        write(&dir.join("plugins/installed_plugins.json"), "{ not json");
        write(&dir.join("settings.json"), "[1,2");
        assert_eq!(capture("missing"), "");
        assert!(capture("ok").starts_with("OK   - ") && capture("ok").contains("claude mcp add"));
        write(&dir.join("plugins/installed_plugins.json"), &json!({ "plugins": null }).to_string());
        write(&dir.join("settings.json"), &json!({ "enabledPlugins": "yes" }).to_string());
        assert_eq!(capture("missing"), "");
        write(&dir.join("plugins/installed_plugins.json"), &json!({ "plugins": { "ai-companion@x": [{}] } }).to_string());
        write(&dir.join("settings.json"), "{}");
        assert!(capture("ok").contains("installed but disabled"));
        assert!(capture("missing").starts_with("WARN"));
        write(&dir.join("settings.json"), &json!({ "enabledPlugins": { "ai-companion@x": true } }).to_string());
        assert!(capture("ok").starts_with("WARN - ") && capture("ok").contains("both"));
    }

    #[test]
    fn plugin_detection_reads_claude_config_dir_when_set_else_the_home_folder() {
        let f = fx();
        let io = io();
        let mut c = ctx(&f, &io, Platform::Mac, None);
        let cfg = f.root.join("cfg");
        write(&cfg.join("plugins/installed_plugins.json"), &json!({ "plugins": { "ai-companion@x": [{}] } }).to_string());
        write(&cfg.join("settings.json"), &json!({ "enabledPlugins": { "ai-companion@x": true } }).to_string());
        c.paths.claude_config_dir_env = Some(cfg.to_string_lossy().into_owned());
        let mut r = Report::new();
        check_plugin_path(&mut r, &c, Some("missing"));
        assert!(text(&r).starts_with("OK   - ") && text(&r).contains("through the ai-companion plugin"));
        c.paths.claude_config_dir_env = None;
        let mut r = Report::new();
        check_plugin_path(&mut r, &c, Some("missing"));
        assert_eq!(text(&r), "", "without the variable ~/.claude is read");
    }

    fn similar(c: &DoctorCtx<'_>, config: &Value, state: &str, model: &str) -> String {
        let mut r = Report::new();
        let how = if state == "ready" { format!("by meaning ({}) and shared words", safe_text(model)) } else { "by shared words only, until the LLM is ready".to_string() };
        check_similar(&mut r, c, config, &how, model);
        text(&r)
    }

    #[test]
    fn check_similar_copes_with_an_empty_or_hostile_config() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        for config in [json!({}), json!(null), json!([]), json!({ "similar": null }), json!({ "similar": "off" }), json!({ "similar": { "enabled": "no" } })] {
            let t = similar(&c, &config, "no-key", "embedding-model");
            assert!(t.contains("Similar past tickets: on"), "{config}: {t}");
        }
        assert!(similar(&c, &json!({ "similar": { "enabled": false } }), "ready", "m").contains("off"));
        assert_eq!(similar(&c, &json!({}), "ready", "evil\nFAIL - forged").split('\n').count(), 1, "a hostile model name can't add lines");
        assert!(similar(&c, &json!({}), "no-key", "m").contains("by shared words only, until the LLM is ready; no local history on this machine yet"));
    }

    #[test]
    fn check_similar_counts_real_vectors_only_and_tells_a_corrupt_file_from_an_older_schema() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let file = c.paths.state_dir.join("history.db");
        std::fs::create_dir_all(&c.paths.state_dir).unwrap();
        const MODEL: &str = "embedding-model";
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, kind TEXT); CREATE TABLE item_vectors (item_id INTEGER, model TEXT, dim INTEGER);").unwrap();
        for i in 1..=3 {
            conn.execute("INSERT INTO items (id, kind) VALUES (?, 'ticket')", [i]).unwrap();
        }
        conn.execute("INSERT INTO item_vectors VALUES (1, ?, 2)", [MODEL]).unwrap();
        conn.execute("INSERT INTO item_vectors VALUES (2, ?, 1)", [MODEL]).unwrap();
        drop(conn);
        assert!(similar(&c, &json!({}), "ready", MODEL).contains("1 of 3 ticket, analysis and PR item(s) embedded"), "the placeholder isn't counted");
        std::fs::write(&file, "this is not a database, just text that is long enough to be read as one".repeat(20)).unwrap();
        let corrupt = similar(&c, &json!({}), "ready", MODEL);
        assert!(corrupt.starts_with("WARN - Similar past tickets: on, ") && corrupt.contains("couldn't be read"), "{corrupt}");
        assert!(!corrupt.contains("next start"));
        std::fs::remove_file(&file).unwrap();
        rusqlite::Connection::open(&file).unwrap().execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, kind TEXT);").unwrap();
        let old = similar(&c, &json!({}), "ready", MODEL);
        assert!(old.starts_with("OK   - Similar past tickets: on, ") && old.contains("older layout") && old.contains("next start"), "{old}");
    }

    // A pack's doctor check flows into the report: a pack's doctor check flows into the report.
    #[tokio::test]
    async fn a_packs_doctor_checks_are_part_of_the_report() {
        let f = fx();
        let io = io();
        let mut c = ctx(&f, &io, Platform::Mac, None);
        c.pack_checks = vec![std::sync::Arc::new(|_| Some(DoctorLine { ok: true, message: "SomeTool: fine.".into() })), std::sync::Arc::new(|_| None)];
        let report = run(&c, Some(json!({ "port": 1, "sharedSecret": "s" }))).await;
        assert!(report.lines.contains(&"OK   - SomeTool: fine.".to_string()), "{}", text(&report));
    }

    #[tokio::test]
    async fn a_missing_config_stops_after_one_fail_with_the_setup_fix() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = run(&c, None).await;
        assert_eq!(r.lines[0], "== Test Product doctor ==\n");
        assert!(r.lines[1].starts_with("FAIL - ") && r.lines[1].contains("setup hasn't been completed"));
        assert_eq!(finish(&mut r, false), 1);
        assert!(text(&r).contains("To fix, start with:") && text(&r).contains("companion setup"));
    }

    fn healthy_config(f: &Fx) -> Value {
        json!({ "port": 4010, "sharedSecret": "secret", "enabledFeatures": ["review-in-editor"], "knownFeatures": ["review-in-editor", "ticket-to-pr"], "repos": { "ACME/ok": format!("/ok/{}", f.root.display()) } })
    }

    #[tokio::test]
    async fn a_healthy_setup_says_everything_looks_good_and_exits_zero() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        write(&c.paths.extension_config_path, &crate::extension_manifest::render_extension_config("secret", 4010, &[]));
        let mut r = run(&c, Some(healthy_config(&f))).await;
        let t = text(&r);
        assert!(t.contains("OK   - Extension connection settings match config.json."), "{t}");
        assert!(t.contains("OK   - Extension manifest matches your servers and features."));
        assert!(t.contains("OK   - Enabled features: review-in-editor."));
        assert!(t.contains("OK   - claudeCli check") && t.contains("FAIL - claudeAuth check (needed by Review PR)"), "{t}");
        assert!(t.contains("Shared test history: not configured"));
        assert!(t.contains("protected-path rule(s)") && t.contains("Claude Bash sandbox: enforced (macOS Seatbelt)."));
        assert!(t.contains("Running in the foreground") && t.contains("The service answers."));
        // The unset MCP token and the failed claudeAuth check are what is left to fix: exit 1.
        assert_eq!(finish(&mut r, false), 1);
        assert_eq!(r.fixes, ["re-run the installer (`install --yes`) or `companion setup`", "claudeAuth check"]);
        let mut clean = Report::new();
        clean.report("OK", "fine", None);
        assert_eq!(finish(&mut clean, false), 0);
        assert!(text(&clean).contains("Everything looks good."));
    }

    #[tokio::test]
    async fn extension_settings_that_differ_or_are_missing_are_failures() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let r = run(&c, Some(healthy_config(&f))).await;
        assert!(text(&r).contains("FAIL - ") && text(&r).contains("companion-config.js is missing"));
        write(&c.paths.extension_config_path, &crate::extension_manifest::render_extension_config("other", 4010, &[]));
        assert!(text(&run(&c, Some(healthy_config(&f))).await).contains("FAIL - The extension's connection settings don't match config.json."));
    }

    #[tokio::test]
    async fn a_down_or_unauthorized_service_says_what_to_do_and_a_background_service_shows_its_pid() {
        let f = fx();
        let mut io = io();
        io.health = health(HealthState::Down, "Nothing is listening on port 4010.");
        let c = ctx(&f, &io, Platform::Mac, None);
        let t = text(&run(&c, Some(healthy_config(&f))).await);
        assert!(t.contains("FAIL - The service isn't running.") && t.contains("Nothing is listening on port 4010. The extension will say \"Could not reach the companion service\"."), "{t}");
        io.health = health(HealthState::Unauthorized, "Something else answers.");
        let c = ctx(&f, &io, Platform::Linux, Some(""));
        let r = run(&c, Some(healthy_config(&f))).await;
        assert!(r.fixes.iter().any(|x| x.contains("lsof -nP -iTCP:4010")));
        assert!(text(&r).contains("Claude Bash sandbox: bwrap, socat not found on PATH"));
        io.launchd_installed = true;
        io.health = health(HealthState::Ok, "up");
        let c = ctx(&f, &io, Platform::Mac, None);
        assert!(text(&run(&c, Some(healthy_config(&f))).await).contains("OK   - Background service is running (pid 42)."));
    }

    #[tokio::test]
    async fn ticket_to_pr_checks_each_repos_origin_without_a_prompt() {
        let f = fx();
        let mut io = io();
        let clone = f.root.join("clone");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        let mut config = healthy_config(&f);
        config["enabledFeatures"] = json!(["ticket-to-pr"]);
        config["repos"] = json!({ "ACME/clone": clone, "ACME/gone": "/nowhere/at/all" });
        for (status, expected) in [(Some(0), "reaches origin without a password prompt"), (Some(2), "origin answered but has no branches yet"), (None, "can't reach origin without a prompt")] {
            io.ls_remote = status;
            let c = ctx(&f, &io, Platform::Mac, None);
            let t = text(&run(&c, Some(config.clone())).await);
            assert!(t.contains(expected), "{t}");
            assert!(t.contains("Ticket to PR: ACME/gone (/nowhere/at/all) isn't a git clone"));
            assert!(t.contains("Create PR moves the ticket with its \"In Review\" transition"));
        }
    }

    #[tokio::test]
    async fn credentials_problems_and_tokens_left_in_config_are_reported() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_credentials(&mut r, &c, &json!({ "jira": { "apiToken": " secret " }, "bitbucket": { "apiToken": "" } }));
        let t = text(&r);
        assert!(t.contains("OK   - Encrypted credentials: none saved yet."), "{t}");
        assert!(t.contains("FAIL - config.json still has apiToken set for: jira "), "{t}");
        assert!(!t.contains("secret"));
        // A loosened credentials file is a warning with the chmod fix.
        write(&c.paths.credentials_path, "x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&c.paths.credentials_path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let mut r = Report::new();
            check_credentials(&mut r, &c, &json!({}));
            assert!(r.fixes.iter().any(|x| x.starts_with("chmod 600 ")), "{}", text(&r));
            assert!(text(&r).contains("credentials.enc permissions: looser than 0600"));
        }
    }

    #[tokio::test]
    async fn borrowed_tokens_are_listed_without_the_token_itself() {
        let f = fx();
        let io = io();
        let mut c = ctx(&f, &io, Platform::Mac, None);
        c.external_sources = &|urls| {
            assert_eq!(urls["jira"], "https://jira.example");
            Map::from_iter([("jira".to_string(), json!("~/.tool/.jira.env"))])
        };
        let mut r = Report::new();
        check_credentials(&mut r, &c, &json!({}));
        assert!(text(&r).contains("OK   - Tokens from some-tool (none saved here): jira <- ~/.tool/.jira.env."), "{}", text(&r));
    }

    #[test]
    fn mcp_registration_reports_a_missing_token_and_skips_claude_code_when_it_is_not_installed() {
        let f = fx();
        let mut io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_mcp_registration(&mut r, &c, &json!({ "port": 4010 }));
        assert_eq!(r.lines, ["WARN - MCP token not created yet."]);
        // With a token saved but no Claude Code, only the skip line shows.
        let store = c.store();
        std::fs::create_dir_all(c.paths.credentials_key_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(c.paths.credentials_path.parent().unwrap()).unwrap();
        store.set_value("mcp.token", "tok").unwrap();
        io.claude = false;
        let c = ctx(&f, &io, Platform::Mac, None);
        let mut r = Report::new();
        check_mcp_registration(&mut r, &c, &json!({ "port": 4010 }));
        assert_eq!(r.lines, ["OK   - Claude Code not installed \u{2014} MCP registration skipped."]);
        assert!(!text(&r).contains("tok"), "never the token");
    }

    #[test]
    fn the_extension_config_is_read_back_from_what_setup_writes() {
        let f = fx();
        let p = f.root.join("c.js");
        write(&p, &crate::extension_manifest::render_extension_config("s3", 4011, &["https://x/*".into()]));
        let v = read_extension_config(&p).unwrap();
        assert_eq!((v["sharedSecret"].clone(), v["serviceBaseUrl"].clone()), (json!("s3"), json!("http://127.0.0.1:4011")));
        write(&p, "not a config");
        assert!(read_extension_config(&p).is_none());
        assert!(read_extension_config(&f.root.join("missing.js")).is_none());
    }

    struct FakeBitbucket {
        pr: Result<Value, String>,
    }

    #[async_trait]
    impl BitbucketProbe for FakeBitbucket {
        fn base_url(&self) -> String {
            "https://bb.example".into()
        }
        // The framework's placeholder profile has no Bitbucket site, so borrow Jira's credential name.
        fn token_name(&self) -> String {
            "jira.apiToken".into()
        }
        async fn application_properties_raw(&self, _: &AuthContext) -> Result<Value, String> {
            Ok(json!({ "version": "9.4.16" }))
        }
        async fn pull_request_raw(&self, _: &AuthContext, _: &str, _: &str, _: i64) -> Result<Value, String> {
            self.pr.clone()
        }
        async fn activity_pages(&self, _: &AuthContext, _: &str, _: &str, _: i64) -> Result<(Vec<Value>, bool), String> {
            Ok((vec![json!({ "values": [{ "action": "COMMENTED", "comment": { "id": 1 }, "commentAnchor": { "path": "a" } }, { "action": "OPENED" }] })], true))
        }
    }

    #[tokio::test]
    async fn the_contract_check_needs_a_token_then_records_the_version_and_diffs_the_pr_fields() {
        let f = fx();
        let io = io();
        let c = ctx(&f, &io, Platform::Mac, None);
        let probe = FakeBitbucket { pr: Ok(json!({ "id": 1 })) };
        let mut r = Report::new();
        run_bitbucket_contract_check(&mut r, &c, &probe, None).await;
        assert_eq!(r.lines, ["== Bitbucket contract check ==\n", "skipped \u{2014} add a Bitbucket token in Settings to run the live contract check"]);

        std::fs::create_dir_all(c.paths.credentials_key_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(c.paths.credentials_path.parent().unwrap()).unwrap();
        c.store().set_value("jira.apiToken", "tok").unwrap();
        let mut r = Report::new();
        run_bitbucket_contract_check(&mut r, &c, &probe, None).await;
        let t = text(&r);
        assert!(t.contains("OK   - Bitbucket version: 9.4.16.") && t.contains("application-properties fields: all present."), "{t}");
        assert!(t.contains("(No PR URL given"));
        let recorded: Value = serde_json::from_str(&std::fs::read_to_string(c.paths.state_dir.join("bitbucket-version.json")).unwrap()).unwrap();
        assert_eq!(recorded["version"], "9.4.16");

        // A moved version is called out; a PR with fields missing is a WARN; a bad url is a FAIL.
        write(&c.paths.state_dir.join("bitbucket-version.json"), &json!({ "version": "9.0.0" }).to_string());
        let mut r = Report::new();
        run_bitbucket_contract_check(&mut r, &c, &probe, Some("https://bb.example/projects/ACME/repos/app/pull-requests/12")).await;
        let t = text(&r);
        assert!(t.contains("Bitbucket changed from 9.0.0 to 9.4.16"), "{t}");
        assert!(t.contains("WARN - pullRequest fields: missing "), "{t}");
        assert!(t.contains("pullRequest optional fields not returned by this Bitbucket version"));
        assert!(t.contains("Hit the activities page cap"));
        assert!(t.contains("activity fields (1 sampled): all present.") && t.contains("commentAnchor fields (1 sampled)"));
        assert_eq!(finish(&mut r, true), i32::from(!r.fixes.is_empty()));
        let mut r = Report::new();
        run_bitbucket_contract_check(&mut r, &c, &probe, Some("nope")).await;
        assert!(text(&r).contains("FAIL - \"nope\" doesn't look like a Bitbucket PR URL"));
        let bad = FakeBitbucket { pr: Err("HTTP 404".into()) };
        let mut r = Report::new();
        run_bitbucket_contract_check(&mut r, &c, &bad, Some("https://bb.example/projects/ACME/repos/app/pull-requests/12")).await;
        assert!(text(&r).contains("FAIL - Could not fetch ACME/app#12: HTTP 404"));
    }

    #[test]
    fn report_lines_are_padded_to_four_and_ok_lines_never_add_fixes() {
        let mut r = Report::new();
        r.report("OK", "a", Some("never"));
        r.report("WARN", "b", Some("fix b"));
        r.report("FAIL", "c", None);
        assert_eq!(r.lines, ["OK   - a", "WARN - b", "FAIL - c"]);
        assert_eq!(r.fixes, ["fix b"]);
        // The closing list drops repeated fixes.
        r.report("WARN", "d", Some("fix b"));
        finish(&mut r, false);
        assert_eq!(text(&r).matches("\u{2022} fix b").count(), 1);
    }
}
