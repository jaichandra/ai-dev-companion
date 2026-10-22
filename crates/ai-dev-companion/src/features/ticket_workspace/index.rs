//! Ticket workspace (port of `features/ticket-workspace/index.ts`): on a Jira
//! ticket, everything this machine knows about it: the local history first
//! (worktrees, PRs, analyses, Claude sessions linked to the key), then git
//! (branches and worktrees whose name has the key: dirty, commits ahead), then the
//! git host (those branches' PRs, and the open PRs' build state). Read-only: it
//! runs on a click, reads, and reports. Its actions only open a local worktree or
//! resume a recorded session.

use super::plan::{build_workspace, history_facts, second_hop_keys, session_keys_of, workspace_summary, HistoryFacts, HistoryPolicy, WorkspaceInput};
use crate::atlassian::AuthContext;
use crate::config::ConfigFn;
use crate::editor::{EditorId, Editors};
use crate::features::ticket_workspace::plan::parse_workspace_payload;
use crate::git_cli::SystemGit;
use crate::history_db::HistoryDb;
use crate::history_record::redact_secrets;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::node_path;
use crate::paths::state_dir;
use crate::prereqs::{is_safe_repo_segment, resolve_review_editor, GitRemoteRules};
use crate::resume_session::{resume_session_in_terminal, ResumeOutcome};
use crate::session_cwd::{session_cwd_allowed, SessionCwdRules};
use crate::terminal::{HeaderSegment, HeaderStyle, Terminal};
use crate::ticket_git::{scan_repo_for_ticket, ticket_worktree_path};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_REPOS: usize = 30;
const MAX_PR_BRANCHES: usize = 10;
const MAX_BUILD_LOOKUPS: usize = 5;

/// The I/O `compute_workspace` does, injectable so a test can run it with no network and no
/// credentials.
#[async_trait]
pub trait WorkspaceIo: Send + Sync {
    /// The git host's base URL; an error is a note, not a failed scan.
    fn git_base_url(&self) -> Result<String, String>;
    /// `{branches, worktrees}` for one clone.
    async fn scan_repo_for_ticket(&self, repo_path: &str, issue_key: &str) -> Result<Value, String>;
    async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>, String>;
    async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>, String>;
    /// The PRs linked to the ticket in the tracker, whatever became of their branch; `None` when
    /// this io can't ask.
    async fn linked_pull_requests(&self, _auth: &AuthContext, _issue_key: &str) -> Option<Result<Vec<Value>, String>> {
        None
    }
}

/// The history store, as this feature reads it.
pub trait WorkspaceHistory: Send + Sync {
    fn get_item(&self, key: &str) -> Result<Option<Value>, String>;
}

impl WorkspaceHistory for HistoryDb {
    fn get_item(&self, key: &str) -> Result<Option<Value>, String> {
        HistoryDb::get_item(self, key)
    }
}

/// The real io: git for the scan, the providers for the host and the tracker.
pub struct SystemWorkspaceIo {
    pub providers: crate::providers::Providers,
    pub home: PathBuf,
}

#[async_trait]
impl WorkspaceIo for SystemWorkspaceIo {
    fn git_base_url(&self) -> Result<String, String> {
        self.providers.git.base_url().map_err(|e| e.to_string())
    }
    async fn scan_repo_for_ticket(&self, repo_path: &str, issue_key: &str) -> Result<Value, String> {
        let scan = scan_repo_for_ticket(Path::new(repo_path), issue_key, &self.home, &SystemGit).await?;
        let worktrees: Vec<Value> = scan.worktrees.iter().map(|w| json!({ "dir": w.dir, "branch": w.branch, "head": w.head, "exists": w.exists, "dirty": w.dirty, "changed": w.changed, "ahead": w.ahead, "behind": w.behind, "isTicketWorktree": w.is_ticket_worktree })).collect();
        Ok(json!({ "branches": scan.branches.iter().map(|b| b.to_json()).collect::<Vec<_>>(), "worktrees": worktrees }))
    }
    async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>, String> {
        self.providers.git.list_branch_pull_requests(auth, project, repo, branch).await.map_err(|e| e.to_string())
    }
    async fn get_commit_build_status(&self, auth: &AuthContext, project: &str, repo: &str, sha: &str) -> Result<Option<Value>, String> {
        self.providers.git.get_commit_build_status(auth, project, repo, sha).await.map_err(|e| e.to_string())
    }
    /// Jira's development panel: the only place a PR whose branch was deleted after the merge can
    /// still be found from the ticket.
    async fn linked_pull_requests(&self, auth: &AuthContext, issue_key: &str) -> Option<Result<Vec<Value>, String>> {
        Some(async {
            let issue = self.providers.issues.get_issue_raw(auth, issue_key, "summary").await.map_err(|e| e.to_string())?;
            let Some(id) = issue["id"].as_str().filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit())) else { return Ok(vec![]) };
            let dev = self.providers.issues.get_linked_pull_requests_raw(auth, id).await.map_err(|e| e.to_string())?;
            let base = self.providers.git.base_url().map_err(|e| e.to_string())?;
            let rules = crate::prereqs::git_remote_rules();
            let prs = crate::features::analyze_issue::plan::linked_pull_requests(&json!({}), &dev, &base, rules);
            Ok(prs.iter().map(|p| serde_json::to_value(p).expect("a linked PR serializes")).collect())
        }
        .await)
    }
}

/// Everything `compute_workspace` needs besides the ticket.
pub struct WorkspaceEnv<'a> {
    pub config: &'a Value,
    pub home: &'a Path,
    pub rules: GitRemoteRules,
    pub history: Option<&'a dyn WorkspaceHistory>,
    pub io: &'a dyn WorkspaceIo,
}

/// The folders this service may open a session or a terminal in: the same allow-list a persisted
/// job file gets.
fn cwd_allowed(config: &Value, home: &Path, cwd: &str) -> bool {
    let repo_paths: Vec<String> = config["repos"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let sessions = state_dir(home).join("sessions").to_string_lossy().into_owned();
    session_cwd_allowed(Some(cwd), &SessionCwdRules { repo_paths: &repo_paths, sessions_root: Some(&sessions), ..Default::default() }, home)
}

/// A history row's worktree is only believed if it is exactly the ticket worktree of a configured
/// repository.
fn history_worktree_ok(config: &Value, home: &Path, issue_key: &str, repo_key: Option<&str>, dir: &str) -> bool {
    let Some(repo_path) = repo_key.and_then(|k| config["repos"].get(k)).and_then(Value::as_str) else { return false };
    ticket_worktree_path(Path::new(repo_path), issue_key, home).is_ok_and(|p| node_path::resolve(dir) == node_path::resolve(&p.to_string_lossy()))
}

fn history_view(env: &WorkspaceEnv<'_>, issue_key: &str) -> Result<Option<HistoryFacts>, String> {
    let Some(history) = env.history else { return Ok(None) };
    let Some(ticket) = history.get_item(&format!("jira:{issue_key}"))? else { return Ok(None) };
    let mut related = Map::new();
    for key in second_hop_keys(&ticket) {
        if let Some(d) = history.get_item(&key)? {
            related.insert(key, d);
        }
    }
    let details: Vec<Value> = related.values().cloned().collect();
    for key in session_keys_of(&details) {
        if let Some(d) = history.get_item(&key)? {
            related.insert(key, d);
        }
    }
    let worktree_ok = |repo_key: Option<&str>, dir: &str| history_worktree_ok(env.config, env.home, issue_key, repo_key, dir);
    let session_ok = |cwd: &str| cwd_allowed(env.config, env.home, cwd);
    Ok(Some(history_facts(Some(&ticket), &related, &HistoryPolicy { worktree_ok: Some(&worktree_ok), session_cwd_ok: Some(&session_ok) })))
}

struct RepoEntry {
    repo_key: String,
    branches: Value,
    worktrees: Value,
    prs: Vec<Value>,
}

/// The workspace for `issue_key_raw`. Never fails for one source failing: a repo git can't read,
/// or a host call that fails, becomes a line in `notes`. Shared by the panel (start) and the MCP
/// tool / `companion resume`.
pub async fn compute_workspace(env: &WorkspaceEnv<'_>, auth: &AuthContext, issue_key_raw: &str, on_progress: &(dyn Fn(&str, &str) + Send + Sync)) -> Result<Value, String> {
    // Every caller (panel, MCP tool, CLI) comes through here: check the key first.
    let issue_key = parse_workspace_payload(&json!({ "issueKey": issue_key_raw }))?;
    let mut notes: Vec<String> = vec![];
    on_progress("history", "Reading the local history\u{2026}");
    let history = match history_view(env, &issue_key) {
        Ok(h) => h,
        Err(e) => {
            notes.push(format!("Local history: {e}"));
            None
        }
    };

    on_progress("git", "Looking for branches and worktrees\u{2026}");
    let mut repos: Vec<RepoEntry> = vec![];
    for (repo_key, repo_path) in env.config["repos"].as_object().into_iter().flatten().take(MAX_REPOS) {
        let Some(repo_path) = repo_path.as_str() else { continue };
        if !Path::new(repo_path).join(".git").exists() {
            continue;
        }
        match env.io.scan_repo_for_ticket(repo_path, &issue_key).await {
            Ok(scan) => {
                if scan["branches"].as_array().is_some_and(|b| !b.is_empty()) || scan["worktrees"].as_array().is_some_and(|w| !w.is_empty()) {
                    repos.push(RepoEntry { repo_key: repo_key.clone(), branches: scan["branches"].clone(), worktrees: scan["worktrees"].clone(), prs: vec![] });
                }
            }
            Err(e) => notes.push(format!("{repo_key}: {e}")),
        }
    }

    on_progress("git", "Checking for pull requests\u{2026}");
    let git_base = match env.io.git_base_url() {
        Ok(b) => Some(b),
        Err(e) => {
            notes.push(format!("Bitbucket: {}", redact_secrets(&e)));
            None
        }
    };
    let (mut pr_lookups, mut build_lookups) = (0usize, 0usize);
    if git_base.is_some() {
        for r in repos.iter_mut() {
            let mut parts = r.repo_key.split('/');
            let (Some(project), Some(repo)) = (parts.next(), parts.next()) else { continue };
            if !is_safe_repo_segment(project) || !is_safe_repo_segment(repo) {
                continue;
            }
            let (project, repo) = (project.to_string(), repo.to_string());
            let remote_branches: Vec<String> = r.branches.as_array().into_iter().flatten().filter(|b| b["remote"] == json!(true)).filter_map(|b| b["name"].as_str().map(String::from)).collect();
            for branch in remote_branches {
                if pr_lookups >= MAX_PR_BRANCHES {
                    break;
                }
                pr_lookups += 1;
                let prs = match env.io.list_branch_pull_requests(auth, &project, &repo, &branch).await {
                    Ok(p) => p,
                    Err(e) => {
                        notes.push(format!("Bitbucket ({} {branch}): {e}", r.repo_key));
                        continue;
                    }
                };
                for mut pr in prs {
                    if pr["state"] == "OPEN" && pr["fromSha"].as_str().is_some_and(|s| !s.is_empty()) && build_lookups < MAX_BUILD_LOOKUPS {
                        build_lookups += 1;
                        match env.io.get_commit_build_status(auth, &project, &repo, pr["fromSha"].as_str().unwrap_or("")).await {
                            Ok(b) => pr["build"] = b.unwrap_or(Value::Null),
                            Err(e) => notes.push(format!("Build status ({} #{}): {e}", r.repo_key, crate::js_text::js_to_string(&pr["id"]))),
                        }
                    }
                    r.prs.push(pr);
                }
            }
        }
    }

    // PRs the tracker links to the ticket that the branch lookup can't reach: a merged PR's branch
    // is usually deleted. Best effort, like every other source here.
    match env.io.linked_pull_requests(auth, &issue_key).await {
        Some(Ok(linked)) => {
            for lp in linked {
                let key = lp["repoKey"].as_str().unwrap_or("").to_lowercase();
                if repos.iter().any(|r| r.repo_key.to_lowercase() == key && r.prs.iter().any(|p| p["id"] == lp["id"])) {
                    continue;
                }
                let idx = match repos.iter().position(|r| r.repo_key.to_lowercase() == key) {
                    Some(i) => i,
                    None => {
                        repos.push(RepoEntry { repo_key: lp["repoKey"].as_str().unwrap_or("").into(), branches: json!([]), worktrees: json!([]), prs: vec![] });
                        repos.len() - 1
                    }
                };
                repos[idx].prs.push(json!({ "id": lp["id"], "title": lp["title"], "state": lp["state"], "url": lp["url"], "fromBranch": null, "toBranch": null, "build": null }));
            }
        }
        Some(Err(e)) => notes.push(format!("Jira linked pull requests: {}", redact_secrets(&e))),
        None => {}
    }

    let repo_values: Vec<Value> = repos.iter().map(|r| json!({ "repoKey": r.repo_key, "branches": r.branches, "worktrees": r.worktrees, "prs": r.prs })).collect();
    let (config, home) = (env.config, env.home);
    Ok(build_workspace(WorkspaceInput {
        issue_key: &issue_key,
        git_base_url: git_base.as_deref().unwrap_or(""),
        rules: env.rules,
        repos: &repo_values,
        history: history.as_ref(),
        notes,
        exists: &|d| Path::new(d).exists(),
        cwd_allowed: &|d| cwd_allowed(config, home, d),
    }))
}

pub struct TicketWorkspace {
    store: Arc<JobStore>,
    config: ConfigFn,
    home: PathBuf,
    rules: GitRemoteRules,
    history: Option<Arc<dyn WorkspaceHistory>>,
    io: Arc<dyn WorkspaceIo>,
    editors: Editors,
    terminal: Terminal,
}

pub struct TicketWorkspaceDeps {
    pub store: Arc<JobStore>,
    pub config: ConfigFn,
    pub home: PathBuf,
    pub rules: GitRemoteRules,
    pub history: Option<Arc<dyn WorkspaceHistory>>,
    pub io: Arc<dyn WorkspaceIo>,
    pub editors: Editors,
    pub terminal: Terminal,
}

pub fn create_ticket_workspace_feature(d: TicketWorkspaceDeps) -> Arc<TicketWorkspace> {
    Arc::new(TicketWorkspace { store: d.store, config: d.config, home: d.home, rules: d.rules, history: d.history, io: d.io, editors: d.editors, terminal: d.terminal })
}

fn patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.update(id, v.as_object().cloned().unwrap_or_default());
}

fn workspace_of(job: &Value) -> FeatureResult<Value> {
    if job["status"] != "awaiting-approval" {
        return Err(FeatureError::JobStatus("Scan the ticket again first \u{2014} this workspace is closed.".into()));
    }
    let ws = &job["data"]["workspace"];
    if !ws.is_object() {
        return Err(FeatureError::JobStatus("This scan has no workspace yet.".into()));
    }
    Ok(ws.clone())
}

impl TicketWorkspace {
    /// The same view without a job (the MCP tool, `companion resume`).
    pub async fn workspace(&self, auth: &AuthContext, issue_key: &str) -> Result<Value, String> {
        let config = (self.config)();
        compute_workspace(&WorkspaceEnv { config: &config, home: &self.home, rules: self.rules, history: self.history.as_deref(), io: self.io.as_ref() }, auth, issue_key, &|_, _| {}).await
    }

    fn editor_for(&self) -> EditorId {
        let config = (self.config)();
        resolve_review_editor(config["reviewEditor"].as_str()).and_then(EditorId::parse).unwrap_or(EditorId::ClaudeCode)
    }
}

#[async_trait]
impl Feature for TicketWorkspace {
    fn id(&self) -> &str {
        "ticket-workspace"
    }

    fn label(&self) -> &str {
        "Ticket workspace"
    }

    /// Polled read-only shape (like analyze-issue): `start` returns the running job; it lands in
    /// awaiting-approval with `data.workspace`.
    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let issue_key = parse_workspace_payload(&payload)?;
        let job = self.store.create("ticket-workspace", Map::from_iter([("issueKey".to_string(), json!(issue_key))]));
        let id = job["id"].as_str().unwrap_or("").to_string();
        let (store, config, home, rules, history, io) = (self.store.clone(), self.config.clone(), self.home.clone(), self.rules, self.history.clone(), self.io.clone());
        let (job_id, key, auth) = (id.clone(), issue_key, ctx.auth);
        tokio::spawn(async move {
            let cfg = config();
            let progress = |step: &str, label: &str| store.set_progress_if_running(&job_id, step, label);
            let env = WorkspaceEnv { config: &cfg, home: &home, rules, history: history.as_deref(), io: io.as_ref() };
            match compute_workspace(&env, &auth, &key, &progress).await {
                Ok(ws) => {
                    let summary = workspace_summary(&ws);
                    patch(&store, &job_id, json!({ "status": "awaiting-approval", "progress": null, "data": { "issueKey": key, "workspace": ws }, "result": { "summary": summary, "files": [] } }));
                }
                Err(e) => patch(&store, &job_id, json!({ "status": "failed", "progress": null, "error": e })),
            }
        });
        Ok(self.store.get(&id).unwrap_or(job))
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::JobStatus(format!("Cannot approve job in status \"{}\" \u{2014} the ticket workspace is a read-only report", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let status = job["status"].as_str().unwrap_or("");
        if status == "awaiting-approval" || status == "failed" {
            patch(&self.store, job["id"].as_str().unwrap_or(""), json!({ "status": "rejected", "progress": null }));
            return Ok(());
        }
        Err(FeatureError::JobStatus(format!("Cannot reject job in status \"{status}\"")))
    }

    fn action_names(&self) -> Vec<&'static str> {
        vec!["open-worktree", "resume-session"]
    }

    async fn run_action(&self, name: &str, job: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        match name {
            // Body { dir }: must be one of this scan's own worktrees.
            "open-worktree" => {
                let dir = ctx.body.as_ref().map(|b| b["dir"].clone()).unwrap_or(Value::Null);
                let ws = workspace_of(&job)?;
                let wt = ws["worktrees"].as_array().into_iter().flatten().find(|w| !dir.is_null() && w["dir"] == dir).cloned();
                let Some(dir) = wt.as_ref().and_then(|w| w["dir"].as_str()) else { return Err(FeatureError::JobStatus("That worktree isn't part of this workspace.".into())) };
                if !Path::new(dir).exists() {
                    return Err(FeatureError::WorktreeGone(format!("{dir} no longer exists.")));
                }
                let editor = self.editor_for();
                self.editors.open_in_editor(dir, editor).await?;
                Ok(json!({ "ok": true, "editor": editor.as_str() }))
            }
            // Body { sessionId }: must be one of this scan's own sessions.
            "resume-session" => {
                let id = ctx.body.as_ref().map(|b| b["sessionId"].clone()).unwrap_or(Value::Null);
                let ws = workspace_of(&job)?;
                let session = ws["sessions"].as_array().into_iter().flatten().find(|s| !id.is_null() && s["id"] == id).cloned();
                let Some(session) = session else { return Err(FeatureError::JobStatus("That session isn't part of this workspace.".into())) };
                let header = vec![vec![HeaderSegment::styled(&format!("Ticket workspace  {}", ws["issueKey"].as_str().unwrap_or("")), HeaderStyle::Title)], vec![HeaderSegment::styled("Resuming Claude Code\u{2026}", HeaderStyle::Status)], vec![]];
                let config = (self.config)();
                match resume_session_in_terminal(&self.terminal, &config, &session, &header, None, &self.home).await? {
                    ResumeOutcome::Resumed => Ok(json!({ "ok": true })),
                    _ => Err(FeatureError::WorktreeGone("That Claude Code session has expired (its folder or transcript is gone).".into())),
                }
            }
            other => Err(FeatureError::Other(format!("No action \"{other}\"."))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::repo_worktrees_root;
    use std::sync::Mutex;
    use GitRemoteRules::BitbucketDc;

    const BB: &str = "https://bb.example";
    const S_EVIL: &str = "33333333-3333-4333-8333-333333333333";
    const S_OK: &str = "44444444-4444-4444-8444-444444444444";

    struct Fx {
        _d: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        config: Value,
    }

    /// Two "clones" (folders with a .git dir); only their existence is checked here.
    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        for name in ["sample-app", "billing"] {
            std::fs::create_dir_all(root.join(name).join(".git")).unwrap();
        }
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let config = json!({ "repos": { "ACME/sample-app": root.join("sample-app"), "ACME/billing": root.join("billing"), "ACME/gone": root.join("gone") } });
        Fx { _d: d, root, home, config }
    }

    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    struct FakeIo {
        calls: Calls,
        home: PathBuf,
        base: Result<String, String>,
        scan_fails_for: Option<&'static str>,
        prs_error: Option<&'static str>,
        linked: Option<Result<Vec<Value>, String>>,
    }

    fn io(f: &Fx) -> FakeIo {
        FakeIo { calls: Arc::new(Mutex::new(vec![])), home: f.home.clone(), base: Ok(BB.into()), scan_fails_for: None, prs_error: None, linked: None }
    }

    impl FakeIo {
        fn note(&self, parts: &[&str]) {
            self.calls.lock().unwrap().push(parts.iter().map(|s| s.to_string()).collect());
        }
        fn calls_of(&self, kind: &str) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().iter().filter(|c| c[0] == kind).cloned().collect()
        }
    }

    #[async_trait]
    impl WorkspaceIo for FakeIo {
        fn git_base_url(&self) -> Result<String, String> {
            self.base.clone()
        }
        async fn scan_repo_for_ticket(&self, repo_path: &str, key: &str) -> Result<Value, String> {
            let name = Path::new(repo_path).file_name().unwrap().to_string_lossy().into_owned();
            self.note(&["scan", &name, key]);
            if self.scan_fails_for == Some(name.as_str()) {
                return Err("not a git repository".into());
            }
            if name != "sample-app" {
                return Ok(json!({ "branches": [], "worktrees": [] }));
            }
            let dir = repo_worktrees_root(Path::new(repo_path), &self.home).join("PROJ-7");
            Ok(json!({
                "branches": [{ "name": "bugfix/PROJ-7-login", "local": true, "remote": true, "ahead": 1, "behind": 0 }],
                "worktrees": [{ "dir": dir, "branch": "bugfix/PROJ-7-login", "exists": true, "dirty": false, "changed": 0, "ahead": 1, "behind": 0, "isTicketWorktree": true }],
            }))
        }
        async fn list_branch_pull_requests(&self, _: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>, String> {
            self.note(&["prs", project, repo, branch]);
            if let Some(e) = self.prs_error {
                return Err(e.into());
            }
            Ok(vec![json!({ "id": 12, "title": "PROJ-7: fix", "state": "OPEN", "fromBranch": branch, "fromSha": "abc1234", "toBranch": "master", "url": format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12"), "updatedAt": 1 })])
        }
        async fn get_commit_build_status(&self, _: &AuthContext, _: &str, _: &str, sha: &str) -> Result<Option<Value>, String> {
            self.note(&["builds", sha]);
            Ok(Some(json!({ "state": "SUCCESSFUL", "counts": { "SUCCESSFUL": 1 }, "builds": [] })))
        }
        async fn linked_pull_requests(&self, _: &AuthContext, _: &str) -> Option<Result<Vec<Value>, String>> {
            self.linked.clone()
        }
    }

    struct FakeHistory {
        rows: Map<String, Value>,
        seen: Mutex<Vec<String>>,
    }

    impl WorkspaceHistory for FakeHistory {
        fn get_item(&self, key: &str) -> Result<Option<Value>, String> {
            self.seen.lock().unwrap().push(key.to_string());
            Ok(self.rows.get(key).cloned())
        }
    }

    fn history(rows: Value) -> Arc<FakeHistory> {
        Arc::new(FakeHistory { rows: rows.as_object().unwrap().clone(), seen: Mutex::new(vec![]) })
    }

    async fn compute(f: &Fx, io: &FakeIo, history: Option<&dyn WorkspaceHistory>, key: &str) -> Result<Value, String> {
        compute_workspace(&WorkspaceEnv { config: &f.config, home: &f.home, rules: BitbucketDc, history, io }, &AuthContext::default(), key, &|_, _| {}).await
    }

    fn ticket_dir(f: &Fx) -> PathBuf {
        repo_worktrees_root(&f.root.join("sample-app"), &f.home).join("PROJ-7")
    }

    #[tokio::test]
    async fn scans_only_real_clones_asks_the_host_only_for_origin_branches_and_adds_build_state_to_open_prs() {
        let f = fx();
        let i = io(&f);
        let ws = compute(&f, &i, None, "PROJ-7").await.unwrap();
        let mut scanned: Vec<String> = i.calls_of("scan").iter().map(|c| c[1].clone()).collect();
        scanned.sort();
        assert_eq!(scanned, ["billing", "sample-app"]);
        assert_eq!(i.calls_of("prs")[0], ["prs", "ACME", "sample-app", "bugfix/PROJ-7-login"]);
        assert_eq!(i.calls_of("builds")[0], ["builds", "abc1234"]);
        assert_eq!((ws["prs"].as_array().unwrap().len(), ws["prs"][0]["build"]["state"].clone()), (1, json!("SUCCESSFUL")));
        assert_eq!(ws["suggestions"]["createPr"], Value::Null, "the worktree's branch already has an open PR");
        assert!(ws["notes"].as_array().unwrap().is_empty());
        assert_eq!(ws["worktrees"][0]["dir"], json!(ticket_dir(&f)));
    }

    #[tokio::test]
    async fn a_git_or_host_failure_becomes_a_note_never_a_failed_scan() {
        let f = fx();
        let mut i = io(&f);
        i.scan_fails_for = Some("billing");
        i.prs_error = Some("Bitbucket rejected the saved API token");
        let ws = compute(&f, &i, None, "PROJ-7").await.unwrap();
        assert_eq!((ws["prs"].as_array().unwrap().len(), ws["worktrees"].as_array().unwrap().len()), (0, 1));
        let notes: Vec<&str> = ws["notes"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(notes.contains(&"ACME/billing: not a git repository"), "{notes:?}");
        assert!(notes.contains(&"Bitbucket (ACME/sample-app bugfix/PROJ-7-login): Bitbucket rejected the saved API token"), "{notes:?}");
        assert_eq!(ws["suggestions"]["createPr"], json!({ "repoKey": "ACME/sample-app", "dir": ticket_dir(&f), "branch": "bugfix/PROJ-7-login" }));
    }

    #[tokio::test]
    async fn the_history_is_read_first_when_there_is_one() {
        let f = fx();
        let h = history(json!({
            "jira:PROJ-7": { "item": { "key": "jira:PROJ-7" }, "edges": [{ "kind": "analysis", "key": "analysis:jira:PROJ-7" }], "facts": [] },
            "analysis:jira:PROJ-7": { "item": { "title": "Analysis of PROJ-7", "excerpt": "Null check", "updatedAt": 3 }, "edges": [], "facts": [] },
        }));
        let mut nothing = f.config.clone();
        nothing["repos"] = json!({});
        let i = io(&f);
        let ws = compute_workspace(&WorkspaceEnv { config: &nothing, home: &f.home, rules: BitbucketDc, history: Some(h.as_ref()), io: &i }, &AuthContext::default(), "PROJ-7", &|_, _| {}).await.unwrap();
        assert_eq!(*h.seen.lock().unwrap(), ["jira:PROJ-7", "analysis:jira:PROJ-7"]);
        assert_eq!((ws["analyses"][0]["excerpt"].clone(), ws["suggestions"]["startFix"].clone()), (json!("Null check"), json!(true)));
    }

    #[tokio::test]
    async fn anything_but_an_issue_key_is_refused_before_touching_history_git_or_the_host() {
        let f = fx();
        let i = io(&f);
        let h = history(json!({}));
        let e = compute(&f, &i, Some(h.as_ref()), "PROJ-7; rm -rf /").await.unwrap_err();
        assert!(e.contains("issueKey"), "{e}");
        assert!(i.calls.lock().unwrap().is_empty() && h.seen.lock().unwrap().is_empty());
    }

    fn tampered(good_cwd: &Path) -> Arc<FakeHistory> {
        history(json!({
            "jira:PROJ-7": { "item": { "key": "jira:PROJ-7" }, "edges": [{ "kind": "worktree", "key": "worktree:/tmp/evil" }, { "kind": "job", "key": "job:x" }], "facts": [] },
            "worktree:/tmp/evil": { "item": { "data": { "dir": "/tmp/evil", "repoKey": "ACME/sample-app", "branch": "b" } }, "edges": [{ "kind": "session", "key": format!("session:{S_EVIL}") }] },
            "job:x": { "item": {}, "edges": [{ "kind": "session", "key": format!("session:{S_OK}") }] },
            format!("session:{S_EVIL}"): { "item": { "updatedAt": 9, "data": { "cwd": "/tmp/evil", "permissionMode": "plan" } }, "edges": [] },
            format!("session:{S_OK}"): { "item": { "updatedAt": 1, "data": { "cwd": good_cwd, "permissionMode": "plan" } }, "edges": [] },
        }))
    }

    #[tokio::test]
    async fn a_tampered_history_row_is_dropped_from_the_scan_with_notes_and_never_reaches_resume() {
        let f = fx();
        let h = tampered(&f.root.join("sample-app"));
        let ws = compute(&f, &io(&f), Some(h.as_ref()), "PROJ-7").await.unwrap();
        assert!(!ws["worktrees"].as_array().unwrap().iter().any(|w| w["dir"] == "/tmp/evil"));
        assert_eq!(ws["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect::<Vec<_>>(), [S_OK]);
        assert!(ws["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("/tmp/evil")));
        assert_ne!(ws["resume"]["worktreeDir"], "/tmp/evil");
        assert_ne!(ws["resume"]["session"]["cwd"], "/tmp/evil");
    }

    fn feature(f: &Fx, history: Option<Arc<dyn WorkspaceHistory>>, io: FakeIo) -> (Arc<TicketWorkspace>, Arc<JobStore>) {
        let store = Arc::new(JobStore::new());
        let config = f.config.clone();
        let feature = create_ticket_workspace_feature(TicketWorkspaceDeps {
            store: store.clone(),
            config: Arc::new(move || config.clone()),
            home: f.home.clone(),
            rules: BitbucketDc,
            history,
            io: Arc::new(io),
            editors: Editors { open_command: "/no/open".into(), code_command: "/no/code".into(), vscode_bundled_cli: "/no/bundled-code".into(), require_macos: false, ..Editors::default() },
            terminal: Terminal { osascript: "/no/osascript".into(), iterm_app: "/no/iterm".into(), require_macos: false, ..Terminal::default() },
        });
        (feature, store)
    }

    async fn settled(store: &JobStore, id: &str) -> Value {
        for _ in 0..400 {
            let j = store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the scan never finished");
    }

    #[tokio::test]
    async fn the_workspaces_own_actions_refuse_a_folder_or_session_that_isnt_in_the_scan() {
        let f = fx();
        let h: Arc<dyn WorkspaceHistory> = tampered(&f.root.join("sample-app"));
        let (feat, store) = feature(&f, Some(h), io(&f));
        let started = feat.start(json!({ "issueKey": "PROJ-7" }), FeatureContext::default()).await.unwrap();
        let job = settled(&store, started["id"].as_str().unwrap()).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert!(job["result"]["summary"].as_str().unwrap().starts_with("PROJ-7: "));
        let act = |name: &'static str, job: Value, body: Value| {
            let feat = feat.clone();
            async move { feat.run_action(name, job, FeatureContext { body: Some(body), ..Default::default() }).await.unwrap_err().to_string() }
        };
        assert_eq!(act("open-worktree", job.clone(), json!({ "dir": "/tmp/evil" })).await, "That worktree isn't part of this workspace.");
        assert_eq!(act("open-worktree", job.clone(), json!({})).await, "That worktree isn't part of this workspace.");
        assert_eq!(act("resume-session", job.clone(), json!({ "sessionId": S_EVIL })).await, "That session isn't part of this workspace.");
        assert_eq!(act("resume-session", job.clone(), json!({ "sessionId": "nope" })).await, "That session isn't part of this workspace.");
        let mut closed = job.clone();
        closed["status"] = json!("rejected");
        assert!(act("open-worktree", closed.clone(), json!({ "dir": "x" })).await.contains("closed"));
        assert!(act("resume-session", closed, json!({ "sessionId": S_OK })).await.contains("closed"));
        assert_eq!(feat.action_names(), ["open-worktree", "resume-session"]);
    }

    #[tokio::test]
    async fn a_host_setting_that_cant_be_read_is_a_note_not_a_failed_scan() {
        let f = fx();
        let mut i = io(&f);
        i.base = Err("bitbucket.baseUrl is not a valid URL".into());
        let ws = compute(&f, &i, None, "PROJ-7").await.unwrap();
        assert!(ws["notes"].as_array().unwrap().iter().any(|n| n == "Bitbucket: bitbucket.baseUrl is not a valid URL"));
        assert_eq!((ws["worktrees"].as_array().unwrap().len(), ws["prs"].as_array().unwrap().len()), (1, 0));
        assert!(i.calls_of("prs").is_empty());
    }

    #[tokio::test]
    async fn a_history_pr_with_a_bad_address_is_replaced_by_the_rebuilt_one_on_the_configured_host() {
        let f = fx();
        let h = history(json!({
            "jira:PROJ-7": { "item": {}, "edges": [{ "kind": "pr", "key": "bitbucket:ACME/sample-app#12" }], "facts": [] },
            "bitbucket:ACME/sample-app#12": { "item": { "title": "t", "url": "javascript:alert(1)" }, "edges": [] },
        }));
        let mut none = f.config.clone();
        none["repos"] = json!({});
        let i = io(&f);
        let ws = compute_workspace(&WorkspaceEnv { config: &none, home: &f.home, rules: BitbucketDc, history: Some(h.as_ref()), io: &i }, &AuthContext::default(), "PROJ-7", &|_, _| {}).await.unwrap();
        assert_eq!((ws["prs"].as_array().unwrap().len(), ws["prs"][0]["url"].clone()), (1, json!(format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12"))));
    }

    #[tokio::test]
    async fn a_pr_the_tracker_links_is_added_even_though_its_branch_is_gone_and_one_the_branch_lookup_found_isnt_doubled() {
        let f = fx();
        let mut i = io(&f);
        i.linked = Some(Ok(vec![
            json!({ "repoKey": "ACME/sample-app", "id": 12, "title": "dup", "state": "OPEN", "url": format!("{BB}/projects/ACME/repos/sample-app/pull-requests/12") }),
            json!({ "repoKey": "ACME/sample-app", "id": 9, "title": "PROJ-7 merged fix", "state": "MERGED", "url": format!("{BB}/projects/ACME/repos/sample-app/pull-requests/9") }),
            json!({ "repoKey": "ACME/other", "id": 3, "title": "elsewhere", "state": "DECLINED", "url": format!("{BB}/projects/ACME/repos/other/pull-requests/3") }),
        ]));
        let ws = compute(&f, &i, None, "PROJ-7").await.unwrap();
        let mut got: Vec<(String, i64, String)> = ws["prs"].as_array().unwrap().iter().map(|p| (p["repoKey"].as_str().unwrap().into(), p["id"].as_i64().unwrap(), p["state"].as_str().unwrap().into())).collect();
        got.sort();
        assert_eq!(got, [("ACME/other".to_string(), 3, "DECLINED".to_string()), ("ACME/sample-app".to_string(), 9, "MERGED".to_string()), ("ACME/sample-app".to_string(), 12, "OPEN".to_string())]);
        assert_eq!(ws["prs"].as_array().unwrap().iter().find(|p| p["id"] == 12).unwrap()["title"], "PROJ-7: fix");
        assert!(ws["notes"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_trackers_linked_prs_failing_is_a_note_and_the_branch_prs_still_show() {
        let f = fx();
        let mut i = io(&f);
        i.linked = Some(Err("No Jira browser session".into()));
        let ws = compute(&f, &i, None, "PROJ-7").await.unwrap();
        assert_eq!(ws["prs"].as_array().unwrap().len(), 1);
        assert!(ws["notes"].as_array().unwrap().iter().any(|n| n == "Jira linked pull requests: No Jira browser session"));
    }

    #[tokio::test]
    async fn the_report_is_read_only_and_a_finished_scan_can_be_discarded() {
        let f = fx();
        let (feat, store) = feature(&f, None, io(&f));
        let started = feat.start(json!({ "issueKey": "proj-7" }), FeatureContext::default()).await.unwrap();
        assert_eq!(started["data"]["issueKey"], "PROJ-7");
        let job = settled(&store, started["id"].as_str().unwrap()).await;
        assert!(feat.approve(job.clone(), FeatureContext::default()).await.unwrap_err().to_string().contains("read-only report"));
        feat.reject(job.clone(), FeatureContext::default()).await.unwrap();
        assert_eq!(store.get(job["id"].as_str().unwrap()).unwrap()["status"], "rejected");
        assert!(feat.reject(store.get(job["id"].as_str().unwrap()).unwrap(), FeatureContext::default()).await.is_err());
        assert!(feat.start(json!({ "issueKey": "nope" }), FeatureContext::default()).await.is_err());
        assert!(feat.workspace(&AuthContext::default(), "PROJ-7").await.is_ok());
    }
}
