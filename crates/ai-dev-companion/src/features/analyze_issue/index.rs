//! Analyze ticket (port of `features/analyze-issue/index.ts`): a headless, read-only Claude run that
//! reads a Jira ticket, picks (or asks the user to confirm) the repository it concerns, reads a
//! read-only checkout of it, and reports a structured analysis as data for the panel. Polled
//! read-only shape: `start` returns the running job at once and it lands in "awaiting-approval"
//! with `data.analysis`. The report can be posted as a Jira comment (browser click only) or boiled
//! down by a small model into a short draft. The session is recorded on the job
//! (`data.claudeSession`) before the run so a failed run can still be continued in a terminal.

use super::plan::{
    analysis_run_options, assemble_summary_comment, build_analysis_prompt, build_comment_summary_prompt, build_repo_pick_prompt, build_summary, candidate_repos_from_ticket, component_mappings_to_learn,
    confirmed_reason, extract_urls, fetch_similar_tickets, format_analysis_as_markdown_comment, is_settled_repo_match, is_strong_repo_match, linked_pull_requests, markdown_to_jira_wiki, parse_analysis,
    parse_issue_key, parse_repo_pick, read_analysis_cache, read_mcp_servers, repo_choice_options, repo_hints_from_similar, similar_query_text, write_analysis_cache, AnalysisPromptInput, ChoiceInput,
    CommentInput, ServerHint, COMMENT_SUMMARY_MODEL, SIMILAR_TIMEOUT,
};
use crate::analysis_checkout::{ensure_analysis_checkout, Checkout};
use crate::atlassian::AuthContext;
use crate::claude::{ClaudeError, ClaudeRunOptions, ClaudeRunner};
use crate::claude_args::Policy;
use crate::git_cli::SystemGit;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, FindSimilar, JobStore};
use crate::packs::ServerHint as PackServerHint;
use crate::prereqs::GitRemoteRules;
use crate::providers::Providers;
use crate::repo_choice::{RepoChoiceError, RepoChoices, RepoOption, RepoQuestion};
use crate::targets::{analyze_targets_from, describe_analyze_scope};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// What this feature needs from the issue tracker and the git host.
#[async_trait]
pub trait IssueApi: Send + Sync {
    async fn get_issue_raw(&self, auth: &AuthContext, key: &str, fields: &str) -> Result<Value, String>;
    async fn get_remote_links_raw(&self, auth: &AuthContext, key: &str) -> Result<Value, String>;
    async fn get_linked_pull_requests_raw(&self, auth: &AuthContext, issue_id: &str) -> Result<Value, String>;
    async fn add_comment(&self, auth: &AuthContext, key: &str, body: &str) -> Result<Option<String>, String>;
    fn git_base_url(&self) -> String;
}

/// The real tracker and git host.
pub struct ProviderIssues(pub Providers);

#[async_trait]
impl IssueApi for ProviderIssues {
    async fn get_issue_raw(&self, auth: &AuthContext, key: &str, fields: &str) -> Result<Value, String> {
        self.0.issues.get_issue_raw(auth, key, fields).await.map_err(|e| e.to_string())
    }
    async fn get_remote_links_raw(&self, auth: &AuthContext, key: &str) -> Result<Value, String> {
        self.0.issues.get_remote_links_raw(auth, key).await.map_err(|e| e.to_string())
    }
    async fn get_linked_pull_requests_raw(&self, auth: &AuthContext, issue_id: &str) -> Result<Value, String> {
        self.0.issues.get_linked_pull_requests_raw(auth, issue_id).await.map_err(|e| e.to_string())
    }
    async fn add_comment(&self, auth: &AuthContext, key: &str, body: &str) -> Result<Option<String>, String> {
        self.0.issues.add_comment(auth, key, body).await.map_err(|e| e.to_string())
    }
    fn git_base_url(&self) -> String {
        self.0.git.base_url().unwrap_or_default()
    }
}

/// The read-only checkout the analysis reads.
#[async_trait]
pub trait Checkouts: Send + Sync {
    async fn ensure(&self, user_clone: &Path, repo_key: &str, root: &Path) -> Result<Checkout, String>;
}

/// The real thing: `git`.
pub struct GitCheckouts;

#[async_trait]
impl Checkouts for GitCheckouts {
    async fn ensure(&self, user_clone: &Path, repo_key: &str, root: &Path) -> Result<Checkout, String> {
        ensure_analysis_checkout(user_clone, repo_key, root, &SystemGit).await
    }
}

pub type ConfigFn = crate::config::ConfigFn;
/// Remembers "component -> repo" in the config; true when it saved.
pub type SaveComponent = Arc<dyn Fn(&str, &str) -> std::io::Result<bool> + Send + Sync>;

pub struct AnalyzeIssueDeps {
    pub store: Arc<JobStore>,
    pub choices: Arc<RepoChoices>,
    pub api: Arc<dyn IssueApi>,
    pub claude: Arc<dyn ClaudeRunner>,
    pub checkouts: Arc<dyn Checkouts>,
    pub config: ConfigFn,
    pub save_component: SaveComponent,
    pub find_similar: Option<Arc<dyn FindSimilar>>,
    pub home: PathBuf,
    /// The folder that holds config.json: the saved analyses live under it.
    pub cache_root: PathBuf,
    pub rules: GitRemoteRules,
    /// The packs' MCP deny list (what a read-only run must never call).
    pub deny_list: Vec<String>,
    pub server_hints: Vec<PackServerHint>,
    /// The user's read-only MCP servers (`read_mcp_servers` over `~/.claude.json`).
    pub mcp_servers: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
}

/// The MCP servers named in `claude_json` (the real source for `AnalyzeIssueDeps::mcp_servers`).
pub fn mcp_servers_from(claude_json: PathBuf) -> Arc<dyn Fn() -> Vec<String> + Send + Sync> {
    Arc::new(move || read_mcp_servers(&claude_json))
}

struct Running {
    cancel: CancellationToken,
    done: Option<JoinHandle<()>>,
}

struct Inner {
    d: AnalyzeIssueDeps,
    running: Mutex<HashMap<String, Running>>,
}

pub struct AnalyzeIssue {
    inner: Arc<Inner>,
}

pub fn create_analyze_issue_feature(d: AnalyzeIssueDeps) -> Arc<AnalyzeIssue> {
    Arc::new(AnalyzeIssue { inner: Arc::new(Inner { d, running: Mutex::new(HashMap::new()) }) })
}

/// Why a run stopped early.
enum Stop {
    Cancelled,
    Failed(String),
}

impl From<String> for Stop {
    fn from(m: String) -> Self {
        Stop::Failed(m)
    }
}

impl From<RepoChoiceError> for Stop {
    fn from(e: RepoChoiceError) -> Self {
        match e {
            RepoChoiceError::Cancelled => Stop::Cancelled,
            other => Stop::Failed(other.to_string()),
        }
    }
}

impl From<ClaudeError> for Stop {
    fn from(e: ClaudeError) -> Self {
        if e.is_aborted() { Stop::Cancelled } else { Stop::Failed(e.to_string()) }
    }
}

fn patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.update(id, v.as_object().cloned().unwrap_or_default());
}

fn comment_draft(issue_key: &str, summary: &str, repo_key: Option<&str>, repo_match: Option<&str>, analysis: &Value) -> String {
    format_analysis_as_markdown_comment(&CommentInput { issue_key: Some(issue_key), summary: Some(summary), repo_key, repo_match, analysis })
}

fn temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("{prefix}{}", crate::ids::uuid_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

impl Inner {
    /// Called before every status/progress update, so a cancelled job never moves on to the next
    /// step or overwrites its "rejecting" state.
    fn checkpoint(cancel: &CancellationToken) -> Result<(), Stop> {
        if cancel.is_cancelled() { Err(Stop::Cancelled) } else { Ok(()) }
    }

    fn progress(&self, job_id: &str, cancel: &CancellationToken, step: &str, label: &str) -> Result<(), Stop> {
        Self::checkpoint(cancel)?;
        patch(&self.d.store, job_id, json!({ "progress": { "stepId": step, "label": label } }));
        Ok(())
    }

    /// Prefer a successful on-disk analysis; otherwise run Claude as usual.
    async fn load_cached_or_analyze(&self, job_id: &str, issue_key: &str, auth: &AuthContext, cancel: &CancellationToken, background: bool, confirm_repo: bool) -> Result<(), Stop> {
        Self::checkpoint(cancel)?;
        let Some(cached) = read_analysis_cache(issue_key, &self.d.cache_root) else {
            return self.run_analyze(job_id, issue_key, auth, cancel, background, confirm_repo).await;
        };
        patch(&self.d.store, job_id, json!({ "progress": { "stepId": "load", "label": "Loading saved analysis\u{2026}" } }));
        // Yield so the extension can paint the brief progress label before we flip to
        // awaiting-approval.
        tokio::time::sleep(Duration::from_millis(50)).await;
        Self::checkpoint(cancel)?;
        let (repo_key, repo_match) = (opt_str(&cached["repoKey"]), opt_str(&cached["repoMatch"]));
        let summary = cached["summary"].as_str().unwrap_or("");
        let draft = comment_draft(cached["issueKey"].as_str().unwrap_or(""), summary, repo_key.as_deref(), repo_match.as_deref(), &cached["analysis"]);
        let mut data = json!({
            "issueKey": cached["issueKey"], "summary": cached["summary"], "repoKey": cached["repoKey"], "repoMatch": cached["repoMatch"],
            "analysis": cached["analysis"], "commentDraftMd": draft, "completedAt": cached["completedAt"], "fromCache": true,
        });
        if !cached["claudeSession"].is_null() {
            data["claudeSession"] = cached["claudeSession"].clone();
        }
        patch(&self.d.store, job_id, json!({
            "status": "awaiting-approval", "progress": null, "data": data,
            "result": { "summary": build_summary(cached["issueKey"].as_str().unwrap_or(""), repo_key.as_deref(), &cached["analysis"]), "files": [] },
        }));
        Ok(())
    }

    async fn run_analyze(&self, job_id: &str, issue_key: &str, auth: &AuthContext, cancel: &CancellationToken, background: bool, confirm_repo: bool) -> Result<(), Stop> {
        let d = &self.d;
        let config = (d.config)();
        let model = config["analyzeIssue"]["model"].as_str().filter(|m| !m.is_empty()).map(String::from);
        let component_map = config["analyzeIssue"]["componentRepoMap"].clone();

        self.progress(job_id, cancel, "fetch", "Reading ticket\u{2026}")?;
        let fields = "summary,description,issuetype,components,labels,priority,versions,fixVersions,environment,comment,issuelinks";
        let mut issue = d.api.get_issue_raw(auth, issue_key, fields).await?;
        Self::checkpoint(cancel)?;
        if !issue.is_object() {
            issue = json!({});
        }
        if issue["key"].as_str().is_none_or(str::is_empty) {
            issue["key"] = json!(issue_key);
        }

        let type_name = issue["fields"]["issuetype"]["name"].as_str().filter(|t| !t.is_empty()).map(String::from);
        let project_key = match issue["fields"]["project"]["key"].as_str().filter(|k| !k.is_empty()) {
            Some(k) => k.to_string(),
            None => parse_issue_key(issue_key)?.1,
        };
        let wanted = analyze_targets_from(&config);
        let scope = describe_analyze_scope(&wanted);
        // No projects configured means every project.
        if !wanted.projects.is_empty() && !wanted.projects.contains(&project_key.to_uppercase()) {
            return Err(Stop::Failed(format!("{issue_key} is in project {project_key}, not one of {} \u{2014} Analyze ticket only runs on {scope}.", wanted.projects.join(", "))));
        }
        if !wanted.issue_types.is_empty() && !type_name.as_ref().is_some_and(|t| wanted.issue_types.contains(t)) {
            return Err(Stop::Failed(format!("{issue_key} is a {}, not one of {} \u{2014} Analyze ticket only runs on {scope}.", type_name.as_deref().unwrap_or("ticket"), wanted.issue_types.join(", "))));
        }

        let remote_links = match d.api.get_remote_links_raw(auth, issue_key).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[analyze-issue] remotelink fetch failed: {e}");
                json!([])
            }
        };
        Self::checkpoint(cancel)?;
        issue["remoteLinks"] = if remote_links.is_array() { remote_links } else { json!([]) };

        // Pull requests already linked to the ticket, from Jira's development panel (plus any PR
        // address in its links and text). Never fatal: the panel is an optional Jira add-on and
        // may be absent or not allowed.
        let mut dev_status = Value::Null;
        if let Some(id) = issue["id"].as_str().filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit())) {
            match d.api.get_linked_pull_requests_raw(auth, id).await {
                Ok(v) => dev_status = v,
                Err(e) => eprintln!("[analyze-issue] linked pull requests unavailable: {e}"),
            }
        }
        Self::checkpoint(cancel)?;
        let linked_prs = linked_pull_requests(&issue, &dev_status, &d.api.git_base_url(), d.rules);

        // Similar past tickets from the local history: fenced, clipped and labelled as untrusted
        // earlier AI output by `build_analysis_prompt`. A background run gets them too (data, not a
        // tool). Never fatal.
        let mut similar_items: Vec<Value> = vec![];
        if let Some(find) = &d.find_similar {
            similar_items = fetch_similar_tickets(Some(find.as_ref()), Some(issue_key), Some(&similar_query_text(&issue)), SIMILAR_TIMEOUT, &|m| eprintln!("[analyze-issue] similar past tickets skipped: {m}")).await;
            Self::checkpoint(cancel)?;
        }

        // A background run can't fetch anything, so its prompt lists no links.
        let run_options = analysis_run_options(background, || (d.mcp_servers)());
        let urls = if run_options.use_urls { extract_urls(&issue) } else { vec![] };

        self.progress(job_id, cancel, "repo", "Identifying repository\u{2026}")?;
        let repos = if config["repos"].is_object() { config["repos"].clone() } else { json!({}) };
        let candidates = candidate_repos_from_ticket(&issue, &repos, &component_map, &linked_prs, d.rules);
        let repo_keys: Vec<String> = repos.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        let hints = repo_hints_from_similar(&similar_items, &repo_keys);

        // What we would pick: a clear ticket-based match, else Claude's pick.
        let mut proposal: Option<String> = None;
        let mut proposal_reason: Option<String> = None;
        if is_strong_repo_match(&candidates) {
            proposal = Some(candidates[0].repo_key.clone());
            proposal_reason = Some(candidates[0].reason.clone());
        } else if !repo_keys.is_empty() {
            self.progress(job_id, cancel, "repo", "Asking Claude to pick a repository\u{2026}")?;
            let pick_cwd = temp_dir("analyze-issue-pick-")?;
            let mut o = ClaudeRunOptions::new(&pick_cwd.to_string_lossy(), &build_repo_pick_prompt(&issue, &repo_keys, &hints, &linked_prs), Policy::NO_TOOLS);
            o.model = model.clone();
            o.cancel = Some(cancel.clone());
            let pick = d.claude.run(o).await;
            let _ = std::fs::remove_dir_all(&pick_cwd);
            let pick = pick?;
            Self::checkpoint(cancel)?;
            proposal = parse_repo_pick(&pick.text, &repo_keys);
            if proposal.is_some() {
                proposal_reason = Some("Claude pick".into());
            }
        }

        let mut repo_key = proposal.clone();
        let mut repo_match_reason = proposal_reason.clone();

        // The user confirms the repo before anything is checked out or analyzed, unless a component
        // mapping they made or confirmed already settles it. A run nobody is watching (a watcher's,
        // or terminal Claude's) can't ask.
        if confirm_repo && !background && !repo_keys.is_empty() && !is_settled_repo_match(&issue, proposal.as_deref(), &component_map) {
            let options: Vec<RepoOption> = repo_choice_options(&ChoiceInput {
                proposal: proposal.as_deref(),
                proposal_reason: proposal_reason.as_deref(),
                candidates: &candidates,
                hint_keys: &hints,
                repo_keys: &repo_keys,
                linked_prs: &linked_prs,
            })
            .into_iter()
            .map(|c| RepoOption { repo_key: c.repo_key, reason: c.reason })
            .collect();
            self.progress(job_id, cancel, "repo", "Waiting for you to confirm the repository\u{2026}")?;
            let question = RepoQuestion { suggested: proposal.clone(), options, prompt: None, no_repo_label: None, null_is_cancel: None };
            let choice = d.choices.ask(job_id, Some(cancel), &question, "run Analyze ticket again.").await?;
            Self::checkpoint(cancel)?;
            repo_match_reason = choice.as_deref().map(|c| confirmed_reason(Some(c), proposal.as_deref(), proposal_reason.as_deref()));
            repo_key = choice;
        }

        // Remember what the user settled on, so the next ticket with this component needs no
        // asking. Never fatal.
        if let Some(rk) = &repo_key {
            let learned = component_mappings_to_learn(&issue, Some(rk), repo_match_reason.as_deref().unwrap_or(""), &component_map);
            for (component, repo) in learned {
                let repo = repo.as_str().unwrap_or("");
                match (d.save_component)(&component, repo) {
                    Ok(true) => println!("[analyze-issue] Learned component \"{component}\" -> {repo} ({}).", repo_match_reason.as_deref().unwrap_or("")),
                    Ok(false) => {}
                    Err(e) => eprintln!("[analyze-issue] could not save component mapping: {e}"),
                }
            }
        }

        // The analysis reads a read-only checkout of the latest default branch kept under the state
        // dir, not the user's own clone (whatever branch and edits it has) and not a worktree
        // folder beside it.
        let mut cwd: Option<String> = None;
        if let Some(rk) = repo_key.clone() {
            let user_clone = repos[rk.as_str()].as_str().unwrap_or("").to_string();
            if user_clone.is_empty() || !Path::new(&user_clone).join(".git").exists() {
                eprintln!("[analyze-issue] repos[\"{rk}\"] = \"{user_clone}\" is not a git checkout; analyzing without repo.");
                repo_key = None;
                repo_match_reason = None;
            } else {
                self.progress(job_id, cancel, "repo", "Getting the latest code from the repository\u{2026}")?;
                let root = crate::paths::state_dir(&d.home).join("repos");
                let reason = repo_match_reason.clone().unwrap_or_default();
                match d.checkouts.ensure(Path::new(&user_clone), &rk, &root).await {
                    Ok(c) => {
                        Self::checkpoint(cancel)?;
                        cwd = Some(c.dir.to_string_lossy().into_owned());
                        repo_match_reason = Some(format!("{reason} \u{b7} origin/{}{}", c.branch, if c.fresh { "" } else { " (could not fetch \u{2014} last copy)" }));
                    }
                    Err(e) => {
                        Self::checkpoint(cancel)?;
                        eprintln!("[analyze-issue] read-only checkout of {rk} failed, using your clone: {e}");
                        cwd = Some(user_clone);
                        repo_match_reason = Some(format!("{reason} \u{b7} your own clone, as it is"));
                    }
                }
            }
        }

        // MCP read tools take free-text arguments (a search is a way out), so a run nobody clicked
        // for gets none.
        let mcp_servers = run_options.mcp_servers.clone();
        let server_hints: Vec<(ServerHint, bool)> = d
            .server_hints
            .iter()
            .map(|h| {
                let available = mcp_servers.iter().any(|s| s.to_lowercase() == h.server.to_lowercase());
                (ServerHint { server: h.server.clone(), line: if available { h.when_available.clone() } else { h.when_missing.clone() } }, available)
            })
            .collect();

        // Keep the live progress label short: MCP server names belong in the report's toolsUsed.
        self.progress(job_id, cancel, "analyze", "Analyzing with Claude\u{2026}")?;

        // No matched repo: keep a per-issue directory under the state dir rather than a temp dir,
        // so claudeSession's cwd is still there for a later phase to --resume into.
        let analysis_cwd = match cwd {
            Some(c) => c,
            None => {
                let dir = crate::paths::state_dir(&d.home).join("sessions").join(issue_key);
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                if repo_match_reason.is_none() {
                    repo_match_reason = Some("no repo matched \u{2014} analyzed without a local clone".into());
                }
                dir.to_string_lossy().into_owned()
            }
        };

        // Own session id (not the repo pick's) so a later phase can --resume this exact transcript
        // in the analysis folder. Stamped BEFORE the run so a run that fails partway through still
        // leaves a resumable session on the job.
        let session_id = crate::ids::uuid_v4();
        let _ = d.store.set_data(job_id, "claudeSession", json!({ "id": session_id, "cwd": analysis_cwd, "permissionMode": "auto" }));
        let hints_for_prompt: Vec<ServerHint> = server_hints.iter().map(|(h, _)| h.clone()).collect();
        let prompt = build_analysis_prompt(&AnalysisPromptInput { issue: &issue, urls: &urls, repo_key: repo_key.as_deref(), mcp_servers: &mcp_servers, server_hints: &hints_for_prompt, background, similar: &similar_items });
        let mut o = ClaudeRunOptions::new(&analysis_cwd, &prompt, if run_options.policy == "readOnlyBackground" { Policy::READ_ONLY_BACKGROUND } else { Policy::READ_ONLY });
        o.model = model;
        o.extra_allowed_tools = run_options.extra_allowed_tools.clone();
        o.disallowed_tools = d.deny_list.clone();
        o.session_id = Some(session_id.clone());
        o.cancel = Some(cancel.clone());
        let (progress_store, progress_id) = (d.store.clone(), job_id.to_string());
        o.on_progress = Some(Arc::new(move |label: &str| progress_store.set_progress_if_running(&progress_id, "analyze", label)));
        let result = d.claude.run(o).await?;
        self.progress(job_id, cancel, "format", "Preparing report\u{2026}")?;
        let mut analysis = parse_analysis(&result.text);
        let claude_session = json!({ "id": result.session_id.clone().unwrap_or(session_id), "cwd": analysis_cwd, "permissionMode": "auto" });

        Self::checkpoint(cancel)?;

        // Also stamp toolsUsed.preferred for the hinted servers Claude left out.
        let raw_only = analysis["raw"].as_str().is_some_and(|r| !r.is_empty()) && !crate::js_text::js_truthy(analysis.get("tldr"));
        if analysis.is_object() && !raw_only {
            let tools_used = analysis["toolsUsed"].as_object().cloned().unwrap_or_default();
            let reported = tools_used.get("preferred").and_then(Value::as_object).cloned().unwrap_or_default();
            if server_hints.iter().any(|(h, _)| reported.get(&h.server).is_none_or(Value::is_null)) {
                let mut preferred: Map<String, Value> = server_hints.iter().map(|(h, a)| (h.server.clone(), json!(a))).collect();
                preferred.extend(reported);
                let mut merged = tools_used.clone();
                merged.insert("preferred".into(), Value::Object(preferred));
                if !tools_used.get("mcpServers").is_some_and(Value::is_array) {
                    merged.insert("mcpServers".into(), json!(mcp_servers));
                }
                analysis["toolsUsed"] = Value::Object(merged);
            }
        }

        let summary = issue["fields"]["summary"].as_str().unwrap_or("").to_string();
        let completed_at = crate::js_date::to_iso_string(crate::job_files::now_ms() as i64).unwrap_or_default();
        let draft = comment_draft(issue_key, &summary, repo_key.as_deref(), repo_match_reason.as_deref(), &analysis);
        let entry = json!({ "issueKey": issue_key, "summary": summary, "repoKey": repo_key, "repoMatch": repo_match_reason, "analysis": analysis, "completedAt": completed_at, "claudeSession": claude_session });
        if let Err(e) = write_analysis_cache(&entry, &d.cache_root) {
            eprintln!("[analyze-issue] cache write failed: {e}");
        }
        patch(&d.store, job_id, json!({
            "status": "awaiting-approval", "progress": null,
            "data": {
                "issueKey": issue_key, "summary": summary, "repoKey": repo_key, "repoMatch": repo_match_reason, "analysis": analysis,
                "commentDraftMd": draft, "completedAt": completed_at, "fromCache": false, "claudeSession": claude_session,
            },
            "result": { "summary": build_summary(issue_key, repo_key.as_deref(), &analysis), "files": [] },
        }));
        Ok(())
    }
}

impl AnalyzeIssue {
    /// `get_issue_analysis` (MCP): the saved analysis, the running job's progress, or a hint.
    pub fn get_issue_analysis(&self, issue_key: &str) -> Value {
        let d = &self.inner.d;
        if let Some(c) = read_analysis_cache(issue_key, &d.cache_root) {
            return json!({ "issueKey": c["issueKey"], "summary": c["summary"], "repoKey": c["repoKey"], "analysis": c["analysis"], "completedAt": c["completedAt"] });
        }
        if let Some(job) = d.store.lookup(&format!("jira:{issue_key}"), "analyze-issue").filter(|j| j["status"] == "running") {
            return json!({ "issueKey": issue_key, "jobId": job["id"], "status": job["status"], "progress": job["progress"]["label"] });
        }
        json!({ "found": false, "hint": "Call analyze_issue to start one." })
    }
}

#[async_trait]
impl Feature for AnalyzeIssue {
    fn id(&self) -> &str {
        "analyze-issue"
    }

    fn label(&self) -> &str {
        "Analyze ticket"
    }

    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let Some(raw_key) = payload["issueKey"].as_str() else { return Err(FeatureError::Other("analyze-issue payload must include issueKey".into())) };
        let (issue_key, _) = parse_issue_key(raw_key)?;
        let force = payload["force"] == json!(true);
        // Terminal Claude's analyze_issue tool sends confirmRepo: false: there is nobody at the
        // browser to answer, so it takes the best match.
        let confirm_repo = payload["confirmRepo"] != json!(false);
        // A watcher's run (set only by the service itself): the stricter background policy and no
        // MCP servers.
        let background = ctx.background;
        let store = &self.inner.d.store;
        let job = store.create("analyze-issue", Map::from_iter([("issueKey".to_string(), json!(issue_key))]));
        let id = job["id"].as_str().unwrap_or("").to_string();
        let cancel = CancellationToken::new();
        let (inner, token, job_id) = (self.inner.clone(), cancel.clone(), id.clone());
        // Deliberately not awaited: the HTTP handler returns the (running) job at once and the
        // extension polls. `force` skips the on-disk cache (Re-analyze).
        let handle = tokio::spawn(async move {
            let outcome = if force {
                inner.run_analyze(&job_id, &issue_key, &ctx.auth, &token, background, confirm_repo).await
            } else {
                inner.load_cached_or_analyze(&job_id, &issue_key, &ctx.auth, &token, background, confirm_repo).await
            };
            match outcome {
                Ok(()) => {}
                // Cancel already flipped the job to "rejecting"; land on rejected with nothing to
                // clean up (no worktree, no pushed state).
                Err(Stop::Cancelled) => patch(&inner.d.store, &job_id, json!({ "status": "rejected", "progress": null })),
                Err(Stop::Failed(_)) if token.is_cancelled() => patch(&inner.d.store, &job_id, json!({ "status": "rejected", "progress": null })),
                Err(Stop::Failed(e)) => patch(&inner.d.store, &job_id, json!({ "status": "failed", "error": e })),
            }
            inner.running.lock().unwrap().remove(&job_id);
        });
        self.inner.running.lock().unwrap().insert(id.clone(), Running { cancel, done: Some(handle) });
        Ok(store.get(&id).unwrap_or(job))
    }

    fn supports_cancel(&self) -> bool {
        true
    }

    async fn cancel(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let id = job["id"].as_str().unwrap_or("").to_string();
        let status = job["status"].as_str().unwrap_or("").to_string();
        let taken = {
            let mut running = self.inner.running.lock().unwrap();
            match running.get_mut(&id) {
                Some(r) if status == "running" => Some((r.cancel.clone(), r.done.take())),
                _ => None,
            }
        };
        let Some((cancel, done)) = taken else { return Err(FeatureError::Other(format!("Cannot cancel job in status \"{status}\""))) };
        patch(&self.inner.d.store, &id, json!({ "status": "rejecting", "progress": { "stepId": "cancel", "label": "Cancelling analysis\u{2026}" } }));
        cancel.cancel();
        if let Some(h) = done {
            let _ = h.await;
        }
        Ok(())
    }

    async fn approve(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        Err(FeatureError::Other(format!("Cannot approve job in status \"{}\" \u{2014} analyze-issue is a read-only report", job["status"].as_str().unwrap_or(""))))
    }

    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        // Cancel-after-finish race: discard the report. Nothing to clean up.
        let status = job["status"].as_str().unwrap_or("");
        if status == "awaiting-approval" || status == "failed" {
            patch(&self.inner.d.store, job["id"].as_str().unwrap_or(""), json!({ "status": "rejected", "progress": null }));
            return Ok(());
        }
        Err(FeatureError::Other(format!("Cannot reject job in status \"{status}\" \u{2014} analyze-issue is a read-only report")))
    }

    fn supports_post_comment(&self) -> bool {
        true
    }

    /// Body must include `{ body: string }` (Markdown from the compose textarea), converted to
    /// Jira wiki markup and posted. Does not change job status: the panel stays open.
    async fn post_comment(&self, job: Value, ctx: FeatureContext) -> FeatureResult<Option<String>> {
        let status = job["status"].as_str().unwrap_or("");
        if status != "awaiting-approval" {
            return Err(FeatureError::Other(format!("Cannot post a comment for a job in status \"{status}\"")));
        }
        let issue_key = job["data"]["issueKey"].as_str().unwrap_or("");
        if issue_key.is_empty() {
            return Err(FeatureError::Other("No issue key on this analysis job".into()));
        }
        let Some(markdown) = ctx.body.as_ref().and_then(|b| b["body"].as_str()) else { return Err(FeatureError::Other("post-comment requires a Markdown \"body\" string".into())) };
        if crate::js_text::js_trim(markdown).is_empty() {
            return Err(FeatureError::Other("Comment body is empty".into()));
        }
        let wiki = markdown_to_jira_wiki(markdown);
        if crate::js_text::js_trim(&wiki).is_empty() {
            return Err(FeatureError::Other("Formatted comment was empty".into()));
        }
        Ok(self.inner.d.api.add_comment(&ctx.auth, issue_key, &wiki).await?.filter(|id| !id.is_empty()))
    }

    fn action_names(&self) -> Vec<&'static str> {
        vec!["confirm-repo", "summarize-comment"]
    }

    async fn run_action(&self, name: &str, job: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        match name {
            // Body { repoKey }: one of the repos offered in job.data.pendingChoice, or null to
            // analyze without a repo.
            "confirm-repo" => self.inner.d.choices.confirm(&job, ctx.body.as_ref()),
            // "Add as comment": the full analysis boiled down to a short draft (under 250 words)
            // by a small model. The full draft stays on job.data as fallback.
            "summarize-comment" => {
                let status = job["status"].as_str().unwrap_or("");
                if status != "awaiting-approval" {
                    return Err(FeatureError::Other(format!("Cannot summarize a comment for a job in status \"{status}\"")));
                }
                let full = job["data"]["commentDraftMd"].as_str().filter(|f| !crate::js_text::js_trim(f).is_empty()).ok_or_else(|| FeatureError::Other("This job has no analysis to summarize".into()))?;
                let issue_key = job["data"]["issueKey"].as_str().unwrap_or("");
                let cwd = temp_dir("analyze-issue-comment-")?;
                let mut o = ClaudeRunOptions::new(&cwd.to_string_lossy(), &build_comment_summary_prompt(full), Policy::NO_TOOLS);
                o.model = Some(COMMENT_SUMMARY_MODEL.into());
                let result = self.inner.d.claude.run(o).await;
                let _ = std::fs::remove_dir_all(&cwd);
                let draft = assemble_summary_comment(Some(issue_key), &result.map_err(|e| e.to_string())?.text);
                if draft.is_empty() {
                    return Err(FeatureError::Other("Claude returned an empty summary".into()));
                }
                Ok(json!({ "draft": draft }))
            }
            other => Err(FeatureError::Other(format!("No action \"{other}\"."))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::testing::FakeClaude;
    use crate::claude::ClaudeRunResult;

    const GOOD: &str = "{\"tldr\":\"Null check missing in login\",\"rootCause\":\"The session is read before it is set.\"}";

    struct Api {
        issue: Value,
        comments: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl IssueApi for Api {
        async fn get_issue_raw(&self, _: &AuthContext, _: &str, _: &str) -> Result<Value, String> {
            Ok(self.issue.clone())
        }
        async fn get_remote_links_raw(&self, _: &AuthContext, _: &str) -> Result<Value, String> {
            Err("no remote links".into())
        }
        async fn get_linked_pull_requests_raw(&self, _: &AuthContext, _: &str) -> Result<Value, String> {
            Err("no development panel".into())
        }
        async fn add_comment(&self, _: &AuthContext, key: &str, body: &str) -> Result<Option<String>, String> {
            self.comments.lock().unwrap().push((key.into(), body.into()));
            Ok(Some("10500".into()))
        }
        fn git_base_url(&self) -> String {
            "https://bitbucket.example.com".into()
        }
    }

    struct FakeCheckouts(Result<Checkout, String>);

    #[async_trait]
    impl Checkouts for FakeCheckouts {
        async fn ensure(&self, _: &Path, _: &str, _: &Path) -> Result<Checkout, String> {
            self.0.clone()
        }
    }

    /// A runner that never answers until it is cancelled.
    struct Hanging;

    #[async_trait]
    impl ClaudeRunner for Hanging {
        async fn run(&self, opts: ClaudeRunOptions) -> Result<ClaudeRunResult, ClaudeError> {
            opts.cancel.expect("a cancel token").cancelled().await;
            Err(ClaudeError::Exec(crate::exec::ExecError::Aborted))
        }
    }

    fn issue(components: &[&str]) -> Value {
        json!({ "id": "10001", "key": "PROJ-7", "fields": {
            "summary": "Login fails", "issuetype": { "name": "Bug" }, "project": { "key": "PROJ" },
            "components": components.iter().map(|c| json!({ "name": c })).collect::<Vec<_>>(),
        } })
    }

    struct Fx {
        feature: Arc<AnalyzeIssue>,
        store: Arc<JobStore>,
        api: Arc<Api>,
        claude: Arc<FakeClaude>,
        learned: Arc<Mutex<Vec<(String, String)>>>,
        root: PathBuf,
        _d: tempfile::TempDir,
    }

    fn fx_with(issue: Value, config: Value, runner: Option<Arc<dyn ClaudeRunner>>, claude: FakeClaude, checkout: Result<Checkout, String>) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let store = Arc::new(JobStore::new());
        let api = Arc::new(Api { issue, comments: Mutex::new(vec![]) });
        let claude = Arc::new(claude);
        let learned = Arc::new(Mutex::new(vec![]));
        let l = learned.clone();
        let feature = create_analyze_issue_feature(AnalyzeIssueDeps {
            store: store.clone(),
            choices: Arc::new(RepoChoices::new(store.clone())),
            api: api.clone(),
            claude: runner.unwrap_or_else(|| claude.clone()),
            checkouts: Arc::new(FakeCheckouts(checkout)),
            config: Arc::new(move || config.clone()),
            save_component: Arc::new(move |c, r| {
                l.lock().unwrap().push((c.into(), r.into()));
                Ok(true)
            }),
            find_similar: None,
            home: root.join("home"),
            cache_root: root.join("cache"),
            rules: GitRemoteRules::BitbucketDc,
            deny_list: vec!["mcp__x__delete".into()],
            server_hints: vec![PackServerHint { server: "wiki".into(), when_available: "Use the wiki.".into(), when_missing: "No wiki.".into() }],
            mcp_servers: Arc::new(|| vec!["jira".into()]),
        });
        Fx { feature, store, api, claude, learned, root, _d: d }
    }

    fn fx(issue: Value, config: Value) -> Fx {
        fx_with(issue, config, None, FakeClaude::replying(GOOD), Err("no checkout".into()))
    }

    async fn settled(store: &JobStore, id: &str) -> Value {
        for _ in 0..600 {
            let j = store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the analysis never finished: {}", store.get(id).unwrap());
    }

    async fn run(f: &Fx, payload: Value) -> Value {
        let job = f.feature.start(payload, FeatureContext::default()).await.unwrap();
        settled(&f.store, job["id"].as_str().unwrap()).await
    }

    #[tokio::test]
    async fn the_payload_must_carry_an_issue_key() {
        let f = fx(issue(&[]), json!({}));
        for p in [json!({}), json!({ "issueKey": 3 }), json!({ "issueKey": "nope" })] {
            assert!(f.feature.start(p, FeatureContext::default()).await.is_err());
        }
        assert!(f.claude.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn with_no_repos_it_analyzes_in_a_per_issue_folder_records_the_session_and_saves_the_report() {
        let f = fx(issue(&[]), json!({ "analyzeIssue": { "projects": [], "issueTypes": [] } }));
        let job = run(&f, json!({ "issueKey": "proj-7" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert_eq!(job["data"]["issueKey"], "PROJ-7");
        assert_eq!(job["data"]["repoKey"], Value::Null);
        assert_eq!(job["data"]["repoMatch"], "no repo matched \u{2014} analyzed without a local clone");
        assert_eq!(job["data"]["fromCache"], false);
        assert!(job["data"]["claudeSession"]["cwd"].as_str().unwrap().ends_with("sessions/PROJ-7"));
        assert_eq!(job["data"]["analysis"]["tldr"], "Null check missing in login");
        assert!(job["data"]["commentDraftMd"].as_str().unwrap().contains("PROJ-7"));
        let calls = f.claude.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, "readOnly");
        // The hinted server is reported as not installed.
        assert_eq!(job["data"]["analysis"]["toolsUsed"]["preferred"], json!({ "wiki": false }));
        // A second click loads the saved report without running Claude.
        let again = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        assert_eq!((again["data"]["fromCache"].clone(), f.claude.calls.lock().unwrap().len()), (json!(true), 1));
        // Re-analyze skips the cache.
        let forced = run(&f, json!({ "issueKey": "PROJ-7", "force": true })).await;
        assert_eq!((forced["data"]["fromCache"].clone(), f.claude.calls.lock().unwrap().len()), (json!(false), 2));
    }

    #[tokio::test]
    async fn a_ticket_outside_the_configured_projects_or_types_fails_before_any_claude_run() {
        let f = fx(issue(&[]), json!({ "analyzeIssue": { "projects": ["OTHER"], "issueTypes": [] } }));
        let job = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        assert_eq!(job["status"], "failed");
        assert!(job["error"].as_str().unwrap().starts_with("PROJ-7 is in project PROJ, not one of OTHER"), "{job}");
        let g = fx(issue(&[]), json!({ "analyzeIssue": { "projects": [], "issueTypes": ["Task"] } }));
        let job = run(&g, json!({ "issueKey": "PROJ-7" })).await;
        assert!(job["error"].as_str().unwrap().starts_with("PROJ-7 is a Bug, not one of Task"), "{job}");
        assert!(f.claude.calls.lock().unwrap().is_empty() && g.claude.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_component_mapping_settles_the_repo_so_nothing_is_asked_and_the_checkout_is_what_claude_reads() {
        let d = tempfile::tempdir().unwrap();
        let clone = d.path().join("app");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        let config = json!({ "repos": { "ACME/app": clone }, "analyzeIssue": { "projects": [], "issueTypes": [], "componentRepoMap": { "Billing": "ACME/app" } } });
        let checkout = Checkout { dir: d.path().join("checkout"), branch: "master".into(), fresh: true };
        let f = fx_with(issue(&["Billing"]), config, None, FakeClaude::replying(GOOD), Ok(checkout));
        let job = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert_eq!(job["data"]["repoKey"], "ACME/app");
        assert!(job["data"]["repoMatch"].as_str().unwrap().ends_with("\u{b7} origin/master"), "{job}");
        assert_eq!(job["data"]["claudeSession"]["cwd"], json!(d.path().join("checkout")));
        assert_eq!(f.claude.calls.lock().unwrap().len(), 1, "no repo pick run was needed");
        assert!(f.learned.lock().unwrap().is_empty(), "an existing mapping is not learned again");
    }

    #[tokio::test]
    async fn an_unsure_repo_is_picked_by_claude_then_confirmed_by_the_user_and_the_component_is_learned() {
        let d = tempfile::tempdir().unwrap();
        let clone = d.path().join("app");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        let config = json!({ "repos": { "ACME/app": clone }, "analyzeIssue": { "projects": [], "issueTypes": [] } });
        let claude = FakeClaude::new(|o| {
            let text = if o.policy.name == "noTools" { "ACME/app" } else { GOOD };
            Ok(ClaudeRunResult { text: text.into(), raw: String::new(), session_id: None, permission_denials: None })
        });
        let checkout = Checkout { dir: d.path().join("checkout"), branch: "main".into(), fresh: false };
        let f = fx_with(issue(&["Billing"]), config, None, claude, Ok(checkout));
        let job = f.feature.start(json!({ "issueKey": "PROJ-7" }), FeatureContext::default()).await.unwrap();
        let id = job["id"].as_str().unwrap().to_string();
        // Wait for the question to be published.
        let mut question = Value::Null;
        for _ in 0..600 {
            question = f.store.get(&id).unwrap()["data"]["pendingChoice"].clone();
            if !question.is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(question["suggested"], "ACME/app");
        let pending = f.store.get(&id).unwrap();
        let answer = f.feature.run_action("confirm-repo", pending, FeatureContext { body: Some(json!({ "repoKey": "ACME/app" })), ..Default::default() }).await.unwrap();
        assert_eq!(answer["id"], json!(id), "{answer}");
        let done = settled(&f.store, &id).await;
        assert_eq!(done["status"], "awaiting-approval", "{done}");
        assert_eq!(done["data"]["repoKey"], "ACME/app");
        let reason = done["data"]["repoMatch"].as_str().unwrap();
        assert!(reason.starts_with("You confirmed it (Claude pick)") && reason.ends_with("(could not fetch \u{2014} last copy)"), "{reason}");
        assert_eq!(*f.learned.lock().unwrap(), [("Billing".to_string(), "ACME/app".to_string())]);
        assert_eq!(done["data"]["pendingChoice"], Value::Null);
    }

    #[tokio::test]
    async fn a_failed_run_is_a_failed_job_and_a_cancelled_one_lands_on_rejected() {
        let f = fx_with(issue(&[]), json!({}), None, FakeClaude::new(|_| Err(ClaudeError::Setup("boom".into()))), Err("x".into()));
        let job = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        assert_eq!((job["status"].clone(), job["error"].clone()), (json!("failed"), json!("boom")));
        // The session was stamped before the run, so a failed run can still be continued.
        assert!(job["data"]["claudeSession"]["id"].is_string());

        let g = fx_with(issue(&[]), json!({}), Some(Arc::new(Hanging)), FakeClaude::replying(GOOD), Err("x".into()));
        let started = g.feature.start(json!({ "issueKey": "PROJ-7" }), FeatureContext::default()).await.unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        for _ in 0..600 {
            if g.store.get(&id).unwrap()["progress"]["stepId"] == "analyze" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        g.feature.cancel(g.store.get(&id).unwrap(), FeatureContext::default()).await.unwrap();
        assert_eq!(g.store.get(&id).unwrap()["status"], "rejected");
        assert!(g.feature.cancel(g.store.get(&id).unwrap(), FeatureContext::default()).await.is_err());
    }

    #[tokio::test]
    async fn the_report_is_read_only_and_can_be_discarded_once_finished() {
        let f = fx(issue(&[]), json!({}));
        let job = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        assert!(f.feature.approve(job.clone(), FeatureContext::default()).await.unwrap_err().to_string().contains("read-only report"));
        f.feature.reject(job.clone(), FeatureContext::default()).await.unwrap();
        let rejected = f.store.get(job["id"].as_str().unwrap()).unwrap();
        assert_eq!(rejected["status"], "rejected");
        assert!(f.feature.reject(rejected, FeatureContext::default()).await.is_err());
    }

    #[tokio::test]
    async fn posting_a_comment_needs_a_finished_job_and_a_markdown_body_and_posts_wiki_markup() {
        let f = fx(issue(&[]), json!({}));
        let job = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        let post = |job: Value, body: Value| {
            let feature = f.feature.clone();
            async move { feature.post_comment(job, FeatureContext { body: Some(body), ..Default::default() }).await }
        };
        assert!(post(job.clone(), json!({})).await.unwrap_err().to_string().contains("\"body\" string"));
        assert_eq!(post(job.clone(), json!({ "body": "  " })).await.unwrap_err().to_string(), "Comment body is empty");
        assert_eq!(post(job.clone(), json!({ "body": "**bold** text" })).await.unwrap(), Some("10500".to_string()));
        assert_eq!(*f.api.comments.lock().unwrap(), [("PROJ-7".to_string(), "*bold* text\n".to_string())]);
        let mut running = job.clone();
        running["status"] = json!("running");
        assert!(post(running, json!({ "body": "x" })).await.unwrap_err().to_string().contains("status \"running\""));
        assert!(f.feature.supports_post_comment());
    }

    #[tokio::test]
    async fn summarize_comment_runs_a_tool_less_small_model_and_assembles_the_draft() {
        let f = fx(issue(&[]), json!({}));
        let job = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        let r = f.feature.run_action("summarize-comment", job.clone(), FeatureContext::default()).await.unwrap();
        assert!(r["draft"].as_str().unwrap().contains("PROJ-7"), "{r}");
        let calls = f.claude.calls.lock().unwrap().clone();
        assert_eq!(calls.last().unwrap().2, "noTools");
        let mut closed = job;
        closed["status"] = json!("rejected");
        assert!(f.feature.run_action("summarize-comment", closed, FeatureContext::default()).await.is_err());
        assert!(f.feature.run_action("nope", Value::Null, FeatureContext::default()).await.is_err());
        let _ = &f.root;
    }

    #[tokio::test]
    async fn get_issue_analysis_answers_from_the_saved_report_or_says_how_to_start_one() {
        let f = fx(issue(&[]), json!({}));
        assert_eq!(f.feature.get_issue_analysis("PROJ-7")["found"], false);
        run(&f, json!({ "issueKey": "PROJ-7" })).await;
        let r = f.feature.get_issue_analysis("PROJ-7");
        assert_eq!((r["issueKey"].clone(), r["analysis"]["tldr"].clone()), (json!("PROJ-7"), json!("Null check missing in login")));
    }
}
