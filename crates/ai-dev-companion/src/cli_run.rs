//! The `companion` command's behaviour (port of `core/cli-run.js`), with every side effect
//! injected through [`CliDeps`]: talking to the service, printing, starting `claude`, checking the
//! filesystem. The binary wires the real ones.

use crate::cli_args::{parse_args, Flag, ParsedArgs};
use crate::cli_format::{self as fmt, one_line, one_line_str};
use crate::git_cli::{Git, GitOut};
use crate::git_hook;
use crate::js_text::slice_utf16;
use crate::resume;
use crate::work_units::WorkUnits;
use crate::worktree_report::{list_worktrees, prune_worktrees};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

pub const USAGE: &str = "Usage: companion <command>\n\
\n\
  status                    Is the service running, and what is it doing?\n\
  jobs [--feature <id>]     List the jobs it is tracking.\n\
  history [<key>]           What it remembers about a ticket, PR or job (PROJ-12, CI/sample-app#7, job:<id>),\n\
                            or, with no key, how long its jobs take. --search <words>, --days <n>, --metrics\n\
                            (the timing table, even with a key).\n\
  similar <key|words…>      Past tickets and PRs in the local history most like one (PROJ-12, CI/sample-app#7) or some words.\n\
  resume <jobId|KEY>        Continue a job's Claude session in this terminal — or, for a ticket\n\
                            (PROJ-1234), its saved session, else Claude Code in its worktree.\n\
  forget <key> [--yes]      Delete one item from its local history.\n\
  open <file:line…>         Open a stack-trace line in Cursor.\n\
  open-shell [--cwd <dir>]  Open a terminal window with a shell in a folder (default: here). macOS only.\n\
  inbox [--all] [--brief]   What the background watchers found for you (--all: seen items too;\n\
                            --brief: one line, or nothing when there is nothing new or the service is down).\n\
  digest                    Your digest: your PRs, reviews waiting on you, your tickets, what's ready.\n\
  precheck [<file>…]        Check changed files against the shared test history. Advisory: it never blocks.\n\
                            With --stdin it reads what git gives a pre-push hook. COMPANION_SKIP=1 skips it.\n\
  hooks install|uninstall|status [<repo>]\n\
                            The optional pre-push check, set in the repo's own git config (runs alongside husky).\n\
  worktrees [--prune [--yes]]\n\
                            Every worktree of your repos: whose it is, its ticket and PR, and whether it is\n\
                            clean, pushed and merged. --prune lists the ones the tool made that are safe to\n\
                            remove (merged or branch gone, and clean; the branches stay); with --yes it removes them.\n\
  doctor                    Check the installation.\n\
  mcp-stdio                 The companion's MCP tools over stdin/stdout, for the Claude Code plugin (not for typing).\n\
\n\
Add --json to status, jobs, history, similar, inbox, digest, precheck and worktrees for scripts.";

const PRECHECK_LIMIT: Duration = Duration::from_millis(3000);
/// The service is down or the command isn't set up (no token / a refused one): as a hook,
/// precheck stays silent about all of these.
static SERVICE_DOWN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"isn't running|Couldn't reach the companion|No MCP token|refused the token").unwrap());
static ISSUE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]*-[1-9]\d*$").unwrap());
static PR_REF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+#[1-9]\d*$").unwrap());
static KIND_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]+:\S+$").unwrap());
const ACTIVE: [&str; 3] = ["running", "approving", "rejecting"];

/// Everything the command does to the outside world.
#[async_trait]
pub trait CliDeps: Send + Sync {
    /// The `git` the worktree commands use.
    type G: Git;
    fn worktree_git(&self) -> &Self::G;

    fn base_url(&self) -> String;
    fn now(&self) -> f64;
    fn home(&self) -> String;
    fn cwd(&self) -> String;
    fn env(&self, name: &str) -> Option<String>;
    fn out(&self, text: &str);
    fn err(&self, text: &str);
    /// Calls one of the service's MCP tools; an error is its message.
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String>;
    fn exists(&self, path: &str) -> bool;
    /// The same allow-list as a job's session folder; the default allows nothing.
    fn cwd_allowed(&self, _cwd: &str) -> bool {
        false
    }
    /// Opens a new terminal window with a shell in `cwd` (macOS only; the default says so).
    async fn open_shell(&self, _cwd: &str) -> Result<(), String> {
        Err("Opening a shell window is only implemented for macOS.".into())
    }
    /// Runs `claude` with `args` in `cwd` in this terminal; its exit code.
    async fn run_claude(&self, args: Vec<String>, cwd: &str) -> i32;
    async fn run_doctor(&self) -> i32;
    /// Runs git for precheck and hooks; `isolated` ignores the user's and system git config.
    async fn git(&self, args: Vec<String>, cwd: &str, isolated: bool) -> GitOut;
    fn read_config(&self) -> Value;
    async fn read_stdin(&self) -> String;
    fn resolve(&self, cwd: &str, path: &str) -> String;
    fn relative(&self, root: &str, path: &str) -> String;
    fn mk_temp(&self) -> Result<String, String>;
    fn rm_temp(&self, dir: &str);
    /// How long `inbox --brief` waits for the service.
    fn brief_timeout(&self) -> Duration {
        Duration::from_millis(3000)
    }
    fn work_units(&self) -> Option<&WorkUnits> {
        None
    }
    fn job_worktrees_root(&self) -> Option<String> {
        None
    }
}

struct Ctx<'a, D: CliDeps> {
    d: &'a D,
    flags: &'a std::collections::BTreeMap<String, Flag>,
    positional: &'a [String],
}

impl<D: CliDeps> Ctx<'_, D> {
    fn on(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    fn value(&self, name: &str) -> Option<&str> {
        match self.flags.get(name) {
            Some(Flag::Value(v)) => Some(v),
            _ => None,
        }
    }
    fn say(&self, text: &str) {
        self.d.out(&format!("{text}\n"));
    }
    fn fail(&self, message: &str, code: i32) -> i32 {
        self.d.err(&format!("companion: {}\n", one_line_str(message)));
        code
    }
    fn usage_error(&self, message: &str) -> i32 {
        self.d.err(&format!("{message}\n\n{USAGE}\n"));
        2
    }
    fn print(&self, value: &Value, text: &str) {
        self.say(&if self.on("json") { serde_json::to_string_pretty(value).expect("a value serializes") } else { text.to_string() });
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn dedupe(files: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    files.into_iter().filter(|f| seen.insert(f.clone())).collect()
}

/// The files a precheck looks at, repo-relative: from git's pre-push stdin, the named files, or
/// (neither) the uncommitted changes.
async fn precheck_files<D: CliDeps>(c: &Ctx<'_, D>, deadline: f64) -> Result<Vec<String>, String> {
    let d = c.d;
    let top = d.git(vec!["rev-parse".into(), "--show-toplevel".into()], &d.cwd(), false).await;
    if top.code != 0 {
        return Err("Run it inside a git repository.".into());
    }
    let root = top.stdout.trim().to_string();
    let mut files: Vec<String> = vec![];
    if c.on("stdin") {
        for update in git_hook::parse_pre_push_lines(&d.read_stdin().await) {
            // git runs synchronously, so timers can't cut in: stop asking once the time is used up.
            if d.now() >= deadline {
                break;
            }
            let Some(args) = git_hook::changed_files_args(&update.local_sha, &update.remote_sha) else { continue };
            let r = d.git(args, &root, false).await;
            if r.code == 0 {
                files.extend(git_hook::parse_name_list(&r.stdout));
            }
        }
    } else if !c.positional.is_empty() {
        for f in c.positional {
            files.push(d.relative(&root, &d.resolve(&d.cwd(), f)));
        }
    } else {
        let r = d.git(vec!["diff".into(), "--name-only".into(), "HEAD".into()], &root, false).await;
        if r.code == 0 {
            files.extend(git_hook::parse_name_list(&r.stdout));
        }
    }
    let mut files = dedupe(files);
    files.truncate(500);
    Ok(files)
}

/// `companion precheck`: always exits 0. As a hook (--stdin) it's silent unless it has a medium or
/// high warning; it stops waiting for the service after about 3 seconds.
async fn precheck<D: CliDeps>(c: &Ctx<'_, D>) -> i32 {
    let d = c.d;
    if d.env("COMPANION_SKIP").as_deref() == Some("1") {
        return 0;
    }
    let config = d.read_config();
    if config["prePush"]["mode"] == "off" {
        return 0;
    }
    let hook = c.on("stdin");
    let work = async {
        let files = precheck_files(c, d.now() + PRECHECK_LIMIT.as_millis() as f64).await?;
        if files.is_empty() {
            return Ok::<Option<Value>, String>(if hook { None } else { Some(json!({ "skipped": true, "reason": "no changed files" })) });
        }
        d.call_tool("get_change_risk", json!({ "files": files })).await.map(Some)
    };
    let result = match tokio::time::timeout(PRECHECK_LIMIT, work).await {
        Err(_) => {
            if !hook {
                d.err("companion precheck: no answer within 3 seconds \u{2014} skipped.\n");
            }
            return 0;
        }
        Ok(Err(message)) => {
            if !hook || !SERVICE_DOWN_RE.is_match(&message) {
                d.err(&format!("companion precheck: {}\n", one_line_str(&message)));
            }
            return 0;
        }
        Ok(Ok(None)) => return 0,
        Ok(Ok(Some(r))) => r,
    };
    if c.on("json") {
        c.say(&serde_json::to_string_pretty(&result).expect("a value serializes"));
        return 0;
    }
    if hook && (result["skipped"].as_bool().unwrap_or(false) || !matches!(s(&result, "level"), "medium" | "high")) {
        return 0;
    }
    d.err(&format!("{}\n", fmt::format_risk(&result)));
    0
}

async fn hooks<D: CliDeps>(c: &Ctx<'_, D>) -> i32 {
    let d = c.d;
    let Some(action) = c.positional.first().map(String::as_str).filter(|a| ["install", "uninstall", "status"].contains(a)) else {
        return c.usage_error("hooks needs install, uninstall or status.");
    };
    let target = c.positional.get(1).map_or(".", String::as_str);
    let top = d.git(vec!["rev-parse".into(), "--show-toplevel".into()], &d.resolve(&d.cwd(), target), false).await;
    if top.code != 0 {
        let shown = c.positional.get(1).cloned().unwrap_or_else(|| d.cwd());
        return c.fail(&format!("{shown} isn't inside a git repository."), 1);
    }
    let repo = top.stdout.trim().to_string();
    if action == "status" {
        let args = match git_hook::status_args(&repo) {
            Ok(a) => a,
            Err(e) => return c.fail(&e, 1),
        };
        let r = d.git(args, &repo, false).await;
        c.say(&if r.code == 0 { format!("The pre-push check is installed in {repo}.") } else { format!("The pre-push check isn't installed in {repo}.") });
        return 0;
    }
    if action == "uninstall" {
        let steps = match git_hook::uninstall_args(&repo) {
            Ok(a) => a,
            Err(e) => return c.fail(&e, 1),
        };
        for args in steps {
            let r = d.git(args, &repo, false).await;
            // 5: the key wasn't set, so there is nothing to remove.
            if r.code != 0 && r.code != 5 {
                let detail = if r.stderr.is_empty() { r.stdout } else { r.stderr };
                return c.fail(format!("git config failed: {detail}").trim(), 1);
            }
        }
        c.say(&format!("Removed the pre-push check from {repo}."));
        return 0;
    }
    let probe_dir = match d.mk_temp() {
        Ok(p) => p,
        Err(e) => return c.fail(&e, 1),
    };
    let mut supported = false;
    let steps = git_hook::probe_steps(&probe_dir);
    if let Ok(steps) = steps {
        let mut last = GitOut { stdout: String::new(), stderr: String::new(), code: 1 };
        for args in steps {
            // Isolated: the user's and system git config must not decide whether this git supports config hooks.
            last = d.git(args, &probe_dir, true).await;
            if last.code != 0 {
                break;
            }
        }
        supported = last.code == 0 && last.stdout.contains(git_hook::PROBE_MARKER);
    }
    d.rm_temp(&probe_dir);
    if !supported {
        c.say(&git_hook::fallback_instructions(&repo));
        return 0;
    }
    let steps = match git_hook::install_args(&repo) {
        Ok(a) => a,
        Err(e) => return c.fail(&e, 1),
    };
    for args in steps {
        let r = d.git(args, &repo, false).await;
        if r.code != 0 {
            let detail = if r.stderr.is_empty() { r.stdout } else { r.stderr };
            return c.fail(format!("git config failed: {detail}").trim(), 1);
        }
    }
    c.say(&format!("Installed the pre-push check in {repo} (git config hook.{}.*). It runs alongside husky and any other pre-push hook, never blocks a push, and COMPANION_SKIP=1 skips it once. `companion hooks uninstall` removes it.", git_hook::HOOK_NAME));
    0
}

/// `companion resume PROJ-1234`: the ticket's saved session if it can still be resumed, else a
/// fresh Claude Code in its worktree. The service already filters what it reports, but a folder is
/// only ever entered here if `cwd_allowed` says so.
async fn resume_ticket<D: CliDeps>(c: &Ctx<'_, D>, issue_key: &str) -> Result<i32, String> {
    let d = c.d;
    let ws = d.call_tool("ticket_workspace", json!({ "issueKey": issue_key })).await?;
    let target = &ws["resume"];
    let session = &target["session"];
    if resume::validate_session(Some(session)) && d.cwd_allowed(s(session, "cwd")) {
        let cwd = s(session, "cwd");
        let transcript = resume::session_transcript_path(&d.home(), &resume::real_cwd(cwd), s(session, "id"));
        if d.exists(cwd) && d.exists(&transcript) {
            c.say(&format!("Resuming the Claude session for {issue_key} in {cwd} \u{2026}"));
            return Ok(d.run_claude(resume::build_resume_args(s(session, "id"), s(session, "permissionMode")), cwd).await);
        }
    }
    if let Some(dir) = target["worktreeDir"].as_str().filter(|w| !w.is_empty() && d.cwd_allowed(w) && d.exists(w)) {
        c.say(&format!("No resumable Claude session for {issue_key} \u{2014} starting Claude Code in its worktree {dir} \u{2026}"));
        return Ok(d.run_claude(vec![], dir).await);
    }
    Ok(c.fail(&format!("Nothing to resume for {issue_key}: no worktree or saved Claude session on this machine. Open the ticket in Chrome and use \u{2728} \u{2192} Ticket workspace."), 1))
}

/// `companion precheck` is advisory and runs inside `git push`: whatever happens it exits 0.
pub async fn main<D: CliDeps, S: AsRef<str>>(argv: &[S], deps: &D) -> i32 {
    if argv.first().map(AsRef::as_ref) != Some("precheck") {
        return run(argv, deps).await;
    }
    run(argv, deps).await;
    0
}

async fn run<D: CliDeps, S: AsRef<str>>(argv: &[S], d: &D) -> i32 {
    let parsed: ParsedArgs = parse_args(argv);
    if !parsed.errors.is_empty() {
        d.err(&format!("{}\n\n{USAGE}\n", parsed.errors.join("\n")));
        return 2;
    }
    let c = Ctx { d, flags: &parsed.flags, positional: &parsed.positional };
    match dispatch(&c, &parsed.command).await {
        Ok(code) => code,
        Err(message) => c.fail(&message, 1),
    }
}

async fn dispatch<D: CliDeps>(c: &Ctx<'_, D>, command: &str) -> Result<i32, String> {
    let d = c.d;
    let positional = c.positional;
    match command {
        "help" => {
            c.say(USAGE);
            Ok(0)
        }
        "doctor" => Ok(d.run_doctor().await),
        "status" => {
            let jobs = d.call_tool("list_jobs", json!({})).await?;
            c.print(&jobs, &fmt::format_status(&jobs, d.now(), &d.base_url()));
            Ok(0)
        }
        "jobs" => {
            let args = c.value("feature").map_or(json!({}), |f| json!({ "featureId": f }));
            let jobs = d.call_tool("list_jobs", args).await?;
            c.print(&jobs, &fmt::format_jobs(&jobs, d.now()));
            Ok(0)
        }
        "history" => {
            if let (Some(key), None, false) = (positional.first(), c.value("search"), c.on("metrics")) {
                let detail = d.call_tool("get_history", json!({ "key": key })).await?;
                c.print(&detail, &fmt::format_history(&detail, d.now()));
                return Ok(0);
            }
            let mut args = json!({});
            if let Some(q) = c.value("search") {
                args["query"] = json!(q);
            }
            if let Some(days) = c.value("days") {
                if days.is_empty() || !days.bytes().all(|b| b.is_ascii_digit()) || days.trim_start_matches('0').is_empty() {
                    return Ok(c.usage_error("--days needs a whole number of days, 1 or more."));
                }
                args["days"] = json!(days.parse::<u64>().map_err(|e| e.to_string())?);
            }
            let data = d.call_tool("list_history", args).await?;
            c.print(&data, &fmt::format_metrics(&data));
            Ok(0)
        }
        "similar" => {
            if positional.is_empty() {
                return Ok(c.usage_error("Similar to what? Give a key (PROJ-12, CI/sample-app#7) or some words."));
            }
            let one = (positional.len() == 1).then(|| positional[0].as_str());
            let is_key = one.is_some_and(|o| ISSUE_KEY_RE.is_match(o) || PR_REF_RE.is_match(o) || KIND_KEY_RE.is_match(o));
            let words = positional.join(" ");
            let args = if is_key { json!({ "key": one }) } else { json!({ "text": slice_utf16(&words, 200) }) };
            let r = d.call_tool("find_similar", args).await?;
            let label = if is_key { one.unwrap_or("").to_string() } else { format!("\"{}\"", slice_utf16(&words, 60)) };
            c.print(&r, &fmt::format_similar(&r, &label));
            Ok(0)
        }
        "resume" => {
            let Some(job_id) = positional.first() else { return Ok(c.usage_error("Which job or ticket? `companion jobs` lists jobs.")) };
            if ISSUE_KEY_RE.is_match(job_id) {
                return resume_ticket(c, &job_id.to_uppercase()).await;
            }
            let job = d.call_tool("get_job", json!({ "jobId": job_id })).await?;
            if ACTIVE.contains(&s(&job, "status")) {
                return Ok(c.fail(&format!("This job is still {} \u{2014} wait for it to finish before resuming.", one_line_str(s(&job, "status"))), 1));
            }
            let session = &job["data"]["claudeSession"];
            if !resume::validate_session(Some(session)) {
                return Ok(c.fail("This job has no resumable Claude Code session.", 1));
            }
            let cwd = s(session, "cwd");
            let transcript = resume::session_transcript_path(&d.home(), &resume::real_cwd(cwd), s(session, "id"));
            if !d.exists(cwd) || !d.exists(&transcript) {
                return Ok(c.fail("That Claude session has expired (its folder or transcript is gone).", 1));
            }
            c.say(&format!("Resuming the {} session in {} \u{2026}", one_line_str(s(&job, "featureId")), one_line_str(cwd)));
            Ok(d.run_claude(resume::build_resume_args(s(session, "id"), s(session, "permissionMode")), cwd).await)
        }
        "forget" => {
            let Some(key) = positional.first() else { return Ok(c.usage_error("Forget which item? Give its key.")) };
            if !c.on("yes") {
                let detail = match d.call_tool("get_history", json!({ "key": key })).await {
                    Ok(detail) => Some(detail),
                    Err(e) if e.contains("Nothing is recorded") => None,
                    Err(e) => return Err(e),
                };
                let Some(detail) = detail.filter(|d| !d.is_null()) else { return Ok(c.fail("Nothing is recorded under that key.", 1)) };
                let title = detail["item"]["title"].as_str().filter(|t| !t.is_empty()).map(|t| format!(" \"{}\"", one_line_str(t))).unwrap_or_default();
                c.say(&format!("Would delete {} ({}){title} and {} link(s).", one_line(Some(&detail["item"]["key"])), one_line(Some(&detail["item"]["kind"])), detail["edges"].as_array().map_or(0, Vec::len)));
                c.say("Re-run with --yes to delete it. This can't be undone.");
                return Ok(2);
            }
            let r = d.call_tool("forget_item", json!({ "key": key })).await?;
            c.say(&format!("Removed {} item(s) and {} timing event(s).", one_line(Some(&r["items"])), one_line(Some(&r["events"]))));
            Ok(0)
        }
        "inbox" => {
            if c.on("brief") {
                // For the Claude Code plugin's SessionStart hook: one line or nothing, never an
                // error, so a session start is never noisy. Time-limited: a hung service must not
                // hold a session start up.
                let brief = match tokio::time::timeout(d.brief_timeout(), d.call_tool("list_notifications", json!({}))).await {
                    Ok(Ok(r)) => fmt::format_inbox_brief(&r),
                    _ => String::new(),
                };
                if !brief.is_empty() {
                    c.say(&brief);
                }
                return Ok(0);
            }
            let r = d.call_tool("list_notifications", if c.on("all") { json!({ "all": true }) } else { json!({}) }).await?;
            c.print(&r, &fmt::format_inbox(&r, d.now()));
            Ok(0)
        }
        "digest" => {
            let digest = d.call_tool("get_digest", json!({})).await?;
            c.print(&digest, &fmt::format_digest(&digest));
            Ok(0)
        }
        "worktrees" => worktrees(c).await,
        "precheck" => Ok(precheck(c).await),
        "hooks" => Ok(hooks(c).await),
        "open" => {
            if positional.is_empty() {
                return Ok(c.usage_error("Open what? Give a location such as src/app/login.ts:42."));
            }
            let r = d.call_tool("open_location", json!({ "text": positional.join(" ") })).await?;
            let column = if crate::js_text::js_truthy(Some(&r["column"])) { format!(":{}", one_line(Some(&r["column"]))) } else { String::new() };
            c.print(&r, &format!("Opened {}: {}:{}{column} in {}.", one_line(Some(&r["repo"])), one_line(Some(&r["path"])), one_line(Some(&r["line"])), one_line(Some(&r["editor"]))));
            Ok(0)
        }
        "open-shell" => {
            let cwd = c.value("cwd").map(|w| d.resolve(&d.cwd(), w)).unwrap_or_else(|| d.cwd());
            if !d.exists(&cwd) {
                return Ok(c.fail(&format!("No such folder: {cwd}"), 1));
            }
            d.open_shell(&cwd).await?;
            c.say(&format!("Opened a shell in {cwd}."));
            Ok(0)
        }
        other => Ok(c.fail(&format!("Unhandled command \"{other}\"."), 2)),
    }
}

async fn worktrees<D: CliDeps>(c: &Ctx<'_, D>) -> Result<i32, String> {
    let d = c.d;
    let config = d.read_config();
    let repos: Vec<(String, PathBuf)> = config["repos"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|p| (k.clone(), PathBuf::from(p)))).collect();
    let units = d.work_units().map(WorkUnits::list).unwrap_or_default();
    let rows = list_worktrees(&repos, &units, d.job_worktrees_root().as_deref(), Path::new(&d.home()), d.worktree_git()).await?;
    if !c.on("prune") {
        let as_json = Value::Array(rows.iter().map(|r| r.to_json()).collect());
        c.print(&as_json, &fmt::format_worktrees(&as_json, d.now()));
        return Ok(0);
    }
    let targets: Vec<_> = rows.iter().filter(|r| r.prunable).collect();
    if targets.is_empty() {
        c.say("Nothing to prune: no worktree made by the tool is merged (or has its branch gone), clean and fully pushed.");
        return Ok(0);
    }
    if !c.on("yes") {
        c.say(&format!("Would remove {} worktree{}:", targets.len(), if targets.len() == 1 { "" } else { "s" }));
        for r in &targets {
            c.say(&format!("  {}  ({}, {})", one_line_str(&r.dir), one_line_str(r.branch.as_deref().filter(|b| !b.is_empty()).unwrap_or("detached")), if r.merged { "merged" } else { "branch gone" }));
        }
        c.say("Re-run with --yes to remove them. The branches stay.");
        return Ok(2);
    }
    let result = prune_worktrees(&rows, d.worktree_git(), d.work_units()).await?;
    for r in &result.removed {
        c.say(&format!("Removed {}", one_line_str(&r.dir)));
    }
    for (row, error) in &result.failed {
        c.say(&format!("Kept {}: {}", one_line_str(&row.dir), one_line_str(error)));
    }
    Ok(i32::from(!result.failed.is_empty()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git_cli::SystemGit;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const SESSION_ID: &str = "3f2b8c1e-1111-4222-8333-444455556666";

    fn session(cwd: &str) -> Value {
        json!({ "id": SESSION_ID, "cwd": cwd, "permissionMode": "plan" })
    }

    /// The injected world: tool answers by name, what was printed and what was called.
    struct H {
        tools: HashMap<String, Result<Value, String>>,
        out: Mutex<Vec<String>>,
        err: Mutex<Vec<String>>,
        calls: Mutex<Vec<Value>>,
        exists: Box<dyn Fn(&str) -> bool + Send + Sync>,
        allow: bool,
        claude_code: i32,
        config: Value,
        home: String,
        units: Option<WorkUnits>,
    }

    fn harness(tools: Vec<(&str, Result<Value, String>)>) -> H {
        H {
            tools: tools.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            out: Mutex::default(),
            err: Mutex::default(),
            calls: Mutex::default(),
            exists: Box::new(|_| true),
            allow: true,
            claude_code: 0,
            config: json!({}),
            home: "/home/me".into(),
            units: None,
        }
    }

    fn ok(v: Value) -> Result<Value, String> {
        Ok(v)
    }

    impl H {
        fn out(&self) -> String {
            self.out.lock().unwrap().join("")
        }
        fn err(&self) -> String {
            self.err.lock().unwrap().join("")
        }
        fn calls(&self) -> Vec<Value> {
            self.calls.lock().unwrap().clone()
        }
        async fn main(&self, argv: &[&str]) -> i32 {
            main(argv, self).await
        }
    }

    #[async_trait]
    impl CliDeps for H {
        type G = SystemGit;
        fn worktree_git(&self) -> &SystemGit {
            &SystemGit
        }
        fn base_url(&self) -> String {
            "http://127.0.0.1:8787".into()
        }
        fn now(&self) -> f64 {
            10_000_000.0
        }
        fn home(&self) -> String {
            self.home.clone()
        }
        fn cwd(&self) -> String {
            "/w".into()
        }
        fn env(&self, _: &str) -> Option<String> {
            None
        }
        fn out(&self, text: &str) {
            self.out.lock().unwrap().push(text.into());
        }
        fn err(&self, text: &str) {
            self.err.lock().unwrap().push(text.into());
        }
        async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push(json!([name, args]));
            self.tools.get(name).cloned().unwrap_or_else(|| Err(format!("unexpected tool {name}")))
        }
        fn exists(&self, path: &str) -> bool {
            (self.exists)(path)
        }
        fn cwd_allowed(&self, cwd: &str) -> bool {
            self.allow && cwd.starts_with("/w/")
        }
        async fn open_shell(&self, cwd: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push(json!(["shell", cwd]));
            Ok(())
        }
        async fn run_claude(&self, args: Vec<String>, cwd: &str) -> i32 {
            self.calls.lock().unwrap().push(json!(["claude", args, cwd]));
            self.claude_code
        }
        async fn run_doctor(&self) -> i32 {
            0
        }
        async fn git(&self, _: Vec<String>, _: &str, _: bool) -> GitOut {
            GitOut { stdout: String::new(), stderr: String::new(), code: 1 }
        }
        fn read_config(&self) -> Value {
            self.config.clone()
        }
        async fn read_stdin(&self) -> String {
            String::new()
        }
        fn resolve(&self, cwd: &str, path: &str) -> String {
            crate::node_path::resolve(&format!("{cwd}/{path}"))
        }
        fn relative(&self, _: &str, path: &str) -> String {
            path.into()
        }
        fn mk_temp(&self) -> Result<String, String> {
            Ok("/tmp/x".into())
        }
        fn rm_temp(&self, _: &str) {}
        fn work_units(&self) -> Option<&WorkUnits> {
            self.units.as_ref()
        }
    }

    #[tokio::test]
    async fn status_lists_what_is_running_jobs_filters_by_feature_and_both_accept_json() {
        let jobs = json!([{ "id": "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "featureId": "analyze-issue", "status": "failed", "updatedAt": 9_000_000 }]);
        let h = harness(vec![("list_jobs", ok(jobs.clone()))]);
        assert_eq!(h.main(&["status"]).await, 0);
        assert!(h.out().contains("running at"));
        assert_eq!(h.main(&["jobs", "--feature", "analyze-issue"]).await, 0);
        assert_eq!(h.calls()[1], json!(["list_jobs", { "featureId": "analyze-issue" }]));
        let j = harness(vec![("list_jobs", ok(jobs.clone()))]);
        j.main(&["jobs", "--json"]).await;
        assert_eq!(serde_json::from_str::<Value>(&j.out()).unwrap(), jobs);
    }

    #[tokio::test]
    async fn history_with_a_key_shows_that_item_with_none_the_timing_metrics_and_search_searches() {
        let h = harness(vec![
            ("get_history", ok(json!({ "item": { "kind": "ticket", "key": "jira:PROJ-7", "title": "T", "updatedAt": 9_000_000 }, "edges": [], "facts": [] }))),
            ("list_history", ok(json!({ "days": 30, "metrics": [], "matches": [] }))),
        ]);
        assert_eq!(h.main(&["history", "PROJ-7"]).await, 0);
        assert_eq!(h.calls()[0], json!(["get_history", { "key": "PROJ-7" }]));
        h.main(&["history"]).await;
        assert_eq!(h.calls()[1], json!(["list_history", {}]));
        h.main(&["history", "--search", "login", "--days", "7"]).await;
        assert_eq!(h.calls()[2], json!(["list_history", { "query": "login", "days": 7 }]));
    }

    fn job(status: &str, data: Value) -> Value {
        json!({ "id": "j", "featureId": "analyze-issue", "status": status, "data": data })
    }

    #[tokio::test]
    async fn resume_runs_claude_resume_in_the_sessions_folder_after_the_same_checks_the_service_makes() {
        let with_session = job("awaiting-approval", json!({ "claudeSession": session("/w/clone") }));
        let h = harness(vec![("get_job", ok(with_session.clone()))]);
        assert_eq!(h.main(&["resume", "j"]).await, 0);
        assert_eq!(h.calls()[1], json!(["claude", ["--resume", SESSION_ID, "--permission-mode", "plan"], "/w/clone"]));

        let running = harness(vec![("get_job", ok(job("running", json!({ "claudeSession": session("/w/clone") }))))]);
        assert_eq!(running.main(&["resume", "j"]).await, 1);
        assert!(running.err().contains("still running"));

        let none = harness(vec![("get_job", ok(job("awaiting-approval", json!({}))))]);
        assert_eq!(none.main(&["resume", "j"]).await, 1);
        assert!(none.err().contains("no resumable"));

        let mut gone = harness(vec![("get_job", ok(with_session))]);
        gone.exists = Box::new(|_| false);
        assert_eq!(gone.main(&["resume", "j"]).await, 1);
        assert!(gone.err().contains("expired"));
    }

    #[tokio::test]
    async fn forget_shows_what_it_would_delete_and_needs_yes_to_do_it() {
        let detail = json!({ "item": { "kind": "ticket", "key": "jira:PROJ-7", "title": "Login", "updatedAt": 9_000_000 }, "edges": [{}, {}], "facts": [] });
        let h = harness(vec![("get_history", ok(detail)), ("forget_item", ok(json!({ "items": 1, "events": 2 })))]);
        assert_eq!(h.main(&["forget", "PROJ-7"]).await, 2);
        assert!(h.out().contains("--yes"));
        assert!(!h.calls().iter().any(|c| c[0] == "forget_item"));
        assert_eq!(h.main(&["forget", "PROJ-7", "--yes"]).await, 0);
        assert!(h.calls().contains(&json!(["forget_item", { "key": "PROJ-7" }])));
        assert!(h.out().contains("Removed 1 item(s) and 2 timing event"));
    }

    #[tokio::test]
    async fn errors_from_the_service_come_out_as_one_plain_line_and_exit_1_bad_usage_exits_2() {
        let h = harness(vec![("list_jobs", Err("The companion service isn't running.".into()))]);
        assert_eq!(h.main(&["status"]).await, 1);
        assert!(h.err().contains("companion: The companion service isn't running"));
        let bad = harness(vec![]);
        assert_eq!(bad.main(&["frobnicate"]).await, 2);
        assert_eq!(bad.main(&["resume"]).await, 2);
        assert_eq!(bad.main(&[]).await, 0);
        assert!(bad.out().contains("Usage:"));
    }

    #[tokio::test]
    async fn forget_reports_a_real_failure_instead_of_nothing_is_recorded() {
        let h = harness(vec![("get_history", Err("The companion service isn't running.".into()))]);
        assert_eq!(h.main(&["forget", "PROJ-7"]).await, 1);
        assert!(h.err().contains("isn't running") && !h.err().contains("Nothing is recorded"));
        let h2 = harness(vec![("get_history", Err("Nothing is recorded under PROJ-7.".into()))]);
        assert_eq!(h2.main(&["forget", "PROJ-7"]).await, 1);
        assert!(h2.err().contains("Nothing is recorded"));
    }

    #[tokio::test]
    async fn history_rejects_a_days_that_is_not_a_whole_number_without_calling_the_service() {
        for bad in ["abc", "0", "1.5"] {
            let h = harness(vec![("list_history", ok(json!([])))]);
            assert_eq!(h.main(&["history", "--days", bad]).await, 2);
            assert!(h.err().contains("--days needs a whole number of days"));
            assert!(h.calls().is_empty());
        }
    }

    #[tokio::test]
    async fn open_sends_the_words_as_one_text_to_open_location_and_prints_where_it_opened() {
        let h = harness(vec![("open_location", ok(json!({ "repo": "ACME/sample-app", "path": "src/app/login.ts", "line": 42, "column": 13, "editor": "cursor" })))]);
        assert_eq!(h.main(&["open", "src/app/login.ts:42:13"]).await, 0);
        assert_eq!(h.calls()[0], json!(["open_location", { "text": "src/app/login.ts:42:13" }]));
        assert!(h.out().contains("Opened ACME/sample-app: src/app/login.ts:42:13 in cursor."));
        let words = harness(vec![("open_location", ok(json!({ "repo": "r", "path": "a.ts", "line": 1, "column": null, "editor": "cursor" })))]);
        assert_eq!(words.main(&["open", "at", "f", "(a.ts:1)"]).await, 0);
        assert_eq!(words.calls(), [json!(["open_location", { "text": "at f (a.ts:1)" }])]);
        let bad = harness(vec![]);
        assert_eq!(bad.main(&["open"]).await, 2);
        assert!(bad.calls().is_empty());
        assert!(bad.err().contains("Open what? Give a location such as src/app/login.ts:42."));
    }

    #[tokio::test]
    async fn open_shell_opens_a_window_in_the_given_folder_or_here_and_refuses_a_missing_one() {
        let mut here = harness(vec![]);
        here.exists = Box::new(|p| p == "/w");
        assert_eq!(here.main(&["open-shell"]).await, 0);
        assert_eq!(here.calls(), [json!(["shell", "/w"])]);
        assert!(here.out().contains("Opened a shell in /w."));
        let mut there = harness(vec![]);
        there.exists = Box::new(|p| p == "/w/sample-app");
        assert_eq!(there.main(&["open-shell", "--cwd", "sample-app"]).await, 0);
        assert_eq!(there.calls(), [json!(["shell", "/w/sample-app"])]);
        let mut gone = harness(vec![]);
        gone.exists = Box::new(|_| false);
        assert_eq!(gone.main(&["open-shell", "--cwd", "nope"]).await, 1);
        assert!(gone.calls().is_empty());
        assert!(gone.err().contains("No such folder: /w/nope"));
    }

    const WT: &str = "/w/sample-app.worktrees/PROJ-7";

    fn ws(target: Value) -> Result<Value, String> {
        ok(json!({ "issueKey": "PROJ-7", "resume": target }))
    }

    #[tokio::test]
    async fn resume_a_ticket_key_resumes_its_session_else_opens_claude_in_its_worktree_else_says_why_not() {
        let target = json!({ "session": session(WT), "worktreeDir": WT });
        let with_session = harness(vec![("ticket_workspace", ws(target.clone()))]);
        assert_eq!(with_session.main(&["resume", "proj-7"]).await, 0);
        assert_eq!(with_session.calls()[0], json!(["ticket_workspace", { "issueKey": "PROJ-7" }]));
        assert_eq!(with_session.calls()[1], json!(["claude", ["--resume", SESSION_ID, "--permission-mode", "plan"], WT]));

        // The transcript is gone: fall back to a fresh session in the worktree.
        let mut expired = harness(vec![("ticket_workspace", ws(target))]);
        expired.exists = Box::new(|p| !p.ends_with(".jsonl"));
        assert_eq!(expired.main(&["resume", "PROJ-7"]).await, 0);
        assert_eq!(expired.calls()[1], json!(["claude", [], WT]));
        assert!(expired.out().contains("No resumable Claude session"));

        let nothing = harness(vec![("ticket_workspace", ws(json!({ "session": null, "worktreeDir": null })))]);
        assert_eq!(nothing.main(&["resume", "PROJ-7"]).await, 1);
        assert!(nothing.err().contains("Nothing to resume for PROJ-7"));
    }

    #[tokio::test]
    async fn resume_a_ticket_key_never_enters_a_folder_the_allow_list_refuses_and_exit_codes_hold() {
        // A tampered response: session and worktree both point at /tmp/evil.
        let evil = harness(vec![("ticket_workspace", ws(json!({ "session": session("/tmp/evil"), "worktreeDir": "/tmp/evil" })))]);
        assert_eq!(evil.main(&["resume", "PROJ-7"]).await, 1);
        assert!(evil.calls().iter().all(|c| c[0] != "claude"));
        assert!(evil.err().contains("Nothing to resume for PROJ-7"));
        // The allow-list says no to everything.
        let mut no_dep = harness(vec![("ticket_workspace", ws(json!({ "session": session(WT), "worktreeDir": WT })))]);
        no_dep.allow = false;
        assert_eq!(no_dep.main(&["resume", "PROJ-7"]).await, 1);
        // An invalid session (bad id) falls back to the worktree, naming the typed key.
        let bad_session = harness(vec![("ticket_workspace", ws(json!({ "session": { "id": "x", "cwd": WT, "permissionMode": "plan" }, "worktreeDir": WT })))]);
        assert_eq!(bad_session.main(&["resume", "proj-7"]).await, 0);
        assert_eq!(bad_session.calls()[1], json!(["claude", [], WT]));
        assert!(bad_session.out().contains("No resumable Claude session for PROJ-7 "));
        // claude's exit code is the command's.
        let mut failing = harness(vec![("ticket_workspace", ws(json!({ "session": session(WT), "worktreeDir": WT })))]);
        failing.claude_code = 3;
        assert_eq!(failing.main(&["resume", "PROJ-7"]).await, 3);
    }

    #[tokio::test]
    async fn resume_with_something_that_is_not_an_issue_key_is_a_job_id_and_with_nothing_a_usage_error() {
        let h = harness(vec![("get_job", ok(job("failed", json!({}))))]);
        assert_eq!(h.main(&["resume", "PROJ-0"]).await, 1);
        assert_eq!(h.calls()[0], json!(["get_job", { "jobId": "PROJ-0" }]));
        let none = harness(vec![]);
        assert_eq!(none.main(&["resume"]).await, 2);
        assert!(none.calls().is_empty());
    }

    fn g(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(cwd).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[tokio::test]
    async fn worktrees_lists_them_prune_only_plans_yes_removes_and_a_bad_flag_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let seed = root.join("seed");
        std::fs::create_dir(&seed).unwrap();
        g(&seed, &["init", "-q", "-b", "master"]);
        g(&seed, &["config", "user.email", "t@example.com"]);
        g(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("a"), "a").unwrap();
        g(&seed, &["add", "a"]);
        g(&seed, &["commit", "-q", "-m", "i"]);
        g(&root, &["clone", "-q", "--bare", seed.to_str().unwrap(), root.join("origin.git").to_str().unwrap()]);
        let clone = root.join("app");
        g(&root, &["clone", "-q", root.join("origin.git").to_str().unwrap(), clone.to_str().unwrap()]);
        let tree = crate::paths::repo_worktrees_root(&clone, &home).join("PROJ-1");
        std::fs::create_dir_all(tree.parent().unwrap()).unwrap();
        g(&clone, &["worktree", "add", "-q", "-b", "PROJ-1", tree.to_str().unwrap(), "origin/master"]);

        let make = || {
            let mut h = harness(vec![]);
            h.config = json!({ "repos": { "ACME/app": clone } });
            h.home = home.to_string_lossy().into_owned();
            h.units = Some(WorkUnits::open(root.join("units.json")));
            h
        };
        let listed = make();
        assert_eq!(listed.main(&["worktrees"]).await, 0);
        let text = listed.out();
        let has = |pattern: &str| Regex::new(pattern).unwrap().is_match(&text);
        assert!(text.contains("ACME/app") && has(r"clone\s+master") && has(r"tool\s+PROJ-1\s+clean, merged") && text.contains("<- can be pruned"), "{text}");

        let json = make();
        json.main(&["worktrees", "--json"]).await;
        assert_eq!(serde_json::from_str::<Value>(&json.out()).unwrap().as_array().unwrap().len(), 2);

        let plan = make();
        assert_eq!(plan.main(&["worktrees", "--prune"]).await, 2);
        assert!(plan.out().contains("Would remove 1 worktree"));
        assert!(tree.exists(), "nothing is removed without --yes");

        let go = make();
        assert_eq!(go.main(&["worktrees", "--prune", "--yes"]).await, 0);
        assert!(go.out().contains("Removed ") && go.out().contains("PROJ-1"));
        assert!(!tree.exists());

        let nothing = make();
        assert_eq!(nothing.main(&["worktrees", "--prune"]).await, 0);
        assert!(nothing.out().contains("Nothing to prune"));

        assert_eq!(make().main(&["worktrees", "--nope"]).await, 2);
    }

    #[tokio::test]
    async fn precheck_is_advisory_it_exits_0_skips_when_asked_and_is_silent_as_a_hook_when_there_is_nothing() {
        // Not in a git repository (the fake git fails): a manual run says so, a hook stays quiet-ish and both exit 0.
        let h = harness(vec![]);
        assert_eq!(h.main(&["precheck"]).await, 0);
        assert!(h.err().contains("Run it inside a git repository."));
        let hook = harness(vec![]);
        assert_eq!(hook.main(&["precheck", "--stdin"]).await, 0);
        assert!(hook.err().contains("Run it inside a git repository."));
        let mut off = harness(vec![]);
        off.config = json!({ "prePush": { "mode": "off" } });
        assert_eq!(off.main(&["precheck"]).await, 0);
        assert!(off.err().is_empty() && off.calls().is_empty());
        // A bad flag is still a clean exit.
        assert_eq!(harness(vec![]).main(&["precheck", "--nope"]).await, 0);
    }

    #[tokio::test]
    async fn inbox_brief_prints_one_line_or_nothing_and_never_an_error() {
        let down = harness(vec![("list_notifications", Err("The companion service isn't running.".into()))]);
        assert_eq!(down.main(&["inbox", "--brief"]).await, 0);
        assert!(down.out().is_empty() && down.err().is_empty());
        let calls = harness(vec![("list_notifications", ok(json!({ "items": [], "unseen": 0 })))]);
        assert_eq!(calls.main(&["inbox", "--all"]).await, 0);
        assert_eq!(calls.calls()[0], json!(["list_notifications", { "all": true }]));
    }

    #[tokio::test]
    async fn similar_sends_a_key_for_a_key_and_the_clipped_words_otherwise() {
        let h = harness(vec![("find_similar", ok(json!({ "enabled": true, "mode": "text", "items": [] })))]);
        assert_eq!(h.main(&["similar", "PROJ-12"]).await, 0);
        assert_eq!(h.calls()[0], json!(["find_similar", { "key": "PROJ-12" }]));
        h.main(&["similar", "CI/sample-app#7"]).await;
        assert_eq!(h.calls()[1], json!(["find_similar", { "key": "CI/sample-app#7" }]));
        h.main(&["similar", "login", "loop"]).await;
        assert_eq!(h.calls()[2], json!(["find_similar", { "text": "login loop" }]));
        assert_eq!(harness(vec![]).main(&["similar"]).await, 2);
    }
}
