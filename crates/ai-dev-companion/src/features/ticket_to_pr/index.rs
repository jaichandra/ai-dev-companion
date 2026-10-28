//! Ticket to PR (port of `features/ticket-to-pr/index.ts`). Start fix (`start`): the repo from the
//! ticket's saved analysis, a branch named from the ticket, the base from the git host's default
//! branch, a persistent worktree `<repo>.worktrees/<KEY>`, and Claude Code in plan mode with the
//! analysis as its prompt. The job then waits in awaiting-approval while you work in that terminal:
//! commit, push and open the pull request there. Once a PR from the branch is open, the review mover
//! moves the ticket to its review status by itself (or on the panel's "Move ticket to In Review") and
//! ends the job as approved. The Create PR path (approve, with a title and description) is still here
//! but the panel no longer offers it. Discarding only stops tracking: the worktree and branch are
//! yours and are never deleted here.

use super::plan::{
    branch_name_for, build_draft_prompt, build_start_fix_prompt, choose_start_fix_repo, default_review_transition, description_with_ticket, fallback_draft, parse_draft, parse_ticket_to_pr_payload,
    run_create_pr_steps, summarize_steps, validate_pr_form, CreatePrDeps, CreatePrInput, TransitionDeps,
};
use super::review_mover::{MoverIo, ReviewMover, POLL_INTERVAL};
use crate::atlassian::AuthContext;
use crate::bitbucket_endpoints::is_safe_branch_name;
use crate::claude::{ClaudeRunOptions, ClaudeRunner};
use crate::claude_args::Policy;
use crate::config::ConfigFn;
use crate::features::analyze_issue::plan::{format_analysis_as_markdown_comment, read_analysis_cache, CommentInput};
use crate::features::ticket_workspace::index::WorkspaceHistory;
use crate::features::ticket_workspace::plan::pr_url_allowed;
use crate::git_cli::Git;
use crate::history_record::redact_secrets;
use crate::jira_workflow::Transition;
use crate::jobs::{Feature, FeatureContext, FeatureError, FeatureResult, JobStore};
use crate::node_path;
use crate::prereqs::GitRemoteRules;
use crate::providers::{GitHost, IssueTracker, NewPullRequest, Providers, ReviewerIds};
use crate::repo_choice::{RepoChoices, RepoOption, RepoQuestion};
use crate::resume_session::{resume_session_located, ResumeOutcome};
use crate::terminal::{HeaderSegment, HeaderStyle, Terminal, TerminalOptions};
use crate::ticket_git::{commit_log, commits_ahead, current_branch, default_branch_from_git, diff_stat, ensure_ticket_worktree, fetch_base, push_branch, ticket_worktree_path};
use crate::work_unit_record::{record_work_unit, record_work_unit_session, Session, Unit};
use crate::work_units::WorkUnits;
use crate::worktree_registry::is_linked_worktree_of;
use crate::worktree_report::{count_owned, WORKTREE_SOFT_LIMIT};
use crate::workspace_resolver::{branch_worktree_path, create_branch_worktree, resolve_workspace, Workspace};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DRAFT_TIMEOUT: Duration = Duration::from_secs(3 * 60);

/// What the approve path and the PR draft need from the git host and the tracker (the mover has its
/// own narrower [`MoverIo`]).
#[async_trait]
pub trait TicketApi: Send + Sync {
    /// The git host's configured address; an error is a malformed or missing setting.
    fn git_base_url(&self) -> Result<String, String>;
    fn issue_url(&self, issue_key: &str) -> String;
    async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>, String>;
    async fn repository_id(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<i64, String>;
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>, String>;
    async fn default_reviewers(&self, auth: &AuthContext, project: &str, repo: &str, ids: &ReviewerIds) -> Result<Vec<String>, String>;
    async fn create_pull_request(&self, auth: &AuthContext, args: &NewPullRequest) -> Result<Value, String>;
    async fn add_remote_link(&self, auth: &AuthContext, issue_key: &str, url: &str, title: &str) -> Result<(), String>;
    async fn list_transitions(&self, auth: &AuthContext, issue_key: &str) -> Result<Vec<Transition>, String>;
    async fn transition_issue(&self, auth: &AuthContext, issue_key: &str, id: &str) -> Result<(), String>;
}

/// The real git host and tracker.
#[derive(Clone)]
pub struct ProviderApi {
    pub git: Arc<dyn GitHost>,
    pub issues: Arc<dyn IssueTracker>,
}

impl ProviderApi {
    pub fn new(p: &Providers) -> Self {
        Self { git: p.git.clone(), issues: p.issues.clone() }
    }
}

fn s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[async_trait]
impl TicketApi for ProviderApi {
    fn git_base_url(&self) -> Result<String, String> {
        self.git.base_url().map_err(s)
    }
    fn issue_url(&self, issue_key: &str) -> String {
        self.issues.issue_url(issue_key)
    }
    async fn list_branch_pull_requests(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Vec<Value>, String> {
        self.git.list_branch_pull_requests(auth, project, repo, branch).await.map_err(s)
    }
    async fn repository_id(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<i64, String> {
        self.git.get_repository_id(auth, project, repo).await.map_err(s)
    }
    async fn who_am_i(&self, auth: &AuthContext) -> Result<Option<String>, String> {
        self.git.who_am_i(auth).await.map_err(s)
    }
    async fn default_reviewers(&self, auth: &AuthContext, project: &str, repo: &str, ids: &ReviewerIds) -> Result<Vec<String>, String> {
        self.git.get_default_reviewers(auth, project, repo, ids).await.map_err(s)
    }
    async fn create_pull_request(&self, auth: &AuthContext, args: &NewPullRequest) -> Result<Value, String> {
        self.git.create_pull_request(auth, args).await.map_err(s)
    }
    async fn add_remote_link(&self, auth: &AuthContext, issue_key: &str, url: &str, title: &str) -> Result<(), String> {
        self.issues.add_remote_link(auth, issue_key, url, title).await.map_err(s)
    }
    async fn list_transitions(&self, auth: &AuthContext, issue_key: &str) -> Result<Vec<Transition>, String> {
        self.issues.list_transitions(auth, issue_key).await.map_err(s)
    }
    async fn transition_issue(&self, auth: &AuthContext, issue_key: &str, id: &str) -> Result<(), String> {
        self.issues.transition_issue(auth, issue_key, id).await.map_err(s)
    }
}

#[async_trait]
impl MoverIo for ProviderApi {
    async fn find_open_pr(&self, auth: &AuthContext, project: &str, repo: &str, branch: &str) -> Result<Option<Value>, String> {
        Ok(self.list_branch_pull_requests(auth, project, repo, branch).await?.into_iter().find(|pr| pr["state"] == "OPEN"))
    }
    async fn list_transitions(&self, auth: &AuthContext, issue_key: &str) -> Result<Vec<Transition>, String> {
        TicketApi::list_transitions(self, auth, issue_key).await
    }
    async fn transition(&self, auth: &AuthContext, issue_key: &str, id: &str) -> Result<(), String> {
        self.transition_issue(auth, issue_key, id).await
    }
}

/// The question put to the user before a worktree is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeQuestion {
    pub prompt: String,
    pub create_label: String,
    pub detail: String,
}

/// Start fix's I/O besides git, injectable so a test runs it with no network, no credentials and no
/// terminal.
#[async_trait]
pub trait StartFixIo: Send + Sync {
    fn read_analysis(&self, issue_key: &str) -> Option<Value>;
    async fn issue_basics(&self, auth: &AuthContext, issue_key: &str) -> Result<Value, String>;
    /// The ticket's raw fields the repo guess reads (components, labels, links).
    async fn issue_for_repo_guess(&self, auth: &AuthContext, issue_key: &str) -> Result<Value, String>;
    async fn default_branch(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<Option<String>, String>;
    /// Where the terminal opened (`TerminalLocation::to_json`), when it said.
    async fn open_terminal(&self, cwd: &str, args: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<Option<Value>, String>;
    /// Asks before a worktree is made; true means go ahead.
    async fn confirm_worktree(&self, job_id: &str, q: &WorktreeQuestion) -> bool;
    /// Reopens a recorded Claude session in a terminal: whether it did, and where it opened.
    async fn resume_session(&self, config: &Value, session: &Value, header: &[Vec<HeaderSegment>], title: Option<String>, dir: &str) -> (bool, Option<Value>);
}

/// The real thing: the providers, the terminal and the repo-choice question.
pub struct RealStartFixIo {
    pub api: ProviderApi,
    pub terminal: Terminal,
    pub choices: Arc<RepoChoices>,
    /// The folder that holds config.json: the saved analyses live under it.
    pub analysis_root: PathBuf,
    pub home: PathBuf,
}

#[async_trait]
impl StartFixIo for RealStartFixIo {
    fn read_analysis(&self, issue_key: &str) -> Option<Value> {
        read_analysis_cache(issue_key, &self.analysis_root)
    }
    async fn issue_basics(&self, auth: &AuthContext, issue_key: &str) -> Result<Value, String> {
        self.api.issues.get_issue_basics(auth, issue_key).await.map_err(s)
    }
    async fn issue_for_repo_guess(&self, auth: &AuthContext, issue_key: &str) -> Result<Value, String> {
        self.api.issues.get_issue_raw(auth, issue_key, "summary,description,components,labels,issuelinks").await.map_err(s)
    }
    async fn default_branch(&self, auth: &AuthContext, project: &str, repo: &str) -> Result<Option<String>, String> {
        self.api.git.get_default_branch(auth, project, repo).await.map_err(s)
    }
    async fn open_terminal(&self, cwd: &str, args: &[String], header: &[Vec<HeaderSegment>], options: &TerminalOptions) -> Result<Option<Value>, String> {
        self.terminal.open_claude_code(cwd, args, header, options).await.map(|l| l.map(|l| l.to_json()))
    }
    async fn confirm_worktree(&self, job_id: &str, q: &WorktreeQuestion) -> bool {
        let question = RepoQuestion {
            suggested: Some(q.create_label.clone()),
            options: vec![RepoOption { repo_key: q.create_label.clone(), reason: q.detail.clone() }],
            prompt: Some(q.prompt.clone()),
            no_repo_label: Some("Cancel".into()),
            null_is_cancel: None,
        };
        matches!(self.choices.ask(job_id, None, &question, "continue again.").await, Ok(Some(_)))
    }
    async fn resume_session(&self, config: &Value, session: &Value, header: &[Vec<HeaderSegment>], title: Option<String>, dir: &str) -> (bool, Option<Value>) {
        match resume_session_located(&self.terminal, config, session, header, title, Some(dir), &self.home).await {
            Ok((ResumeOutcome::Resumed, loc)) => (true, loc.map(|l| l.to_json())),
            _ => (false, None),
        }
    }
}

/// A pull request reference whose address is only kept if it's a PR page on the configured git host
/// (a job file and the host's answer are untrusted). `None` for no PR; an error when the host's
/// address setting can't be read.
pub fn safe_pr_ref(pr: &Value, api: &dyn TicketApi, rules: GitRemoteRules) -> Result<Option<Value>, String> {
    if !pr.is_object() {
        return Ok(None);
    }
    let base = api.git_base_url()?;
    let ok = pr["url"].as_str().is_some_and(|u| pr_url_allowed(u, &base, rules));
    let mut out = pr.clone();
    out["url"] = if ok { pr["url"].clone() } else { Value::Null };
    Ok(Some(out))
}

/// `safe_pr_ref`, but a setting that can't be read means "no address" rather than an error (this
/// runs on every read of the job).
fn safe_or_null(pr: &Value, api: &dyn TicketApi, rules: GitRemoteRules) -> Option<Value> {
    safe_pr_ref(pr, api, rules).unwrap_or_else(|_| pr.is_object().then(|| json!({ "id": pr["id"], "url": null, "title": pr.get("title").cloned().unwrap_or(Value::Null) })))
}

fn split_repo_key(repo_key: &str) -> (&str, &str) {
    let mut parts = repo_key.split('/');
    (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
}

fn patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.update(id, v.as_object().cloned().unwrap_or_default());
}

fn data_patch(store: &JobStore, id: &str, v: Value) {
    let _ = store.patch_data(id, v.as_object().cloned().unwrap_or_default());
}

fn job_err(m: impl Into<String>) -> FeatureError {
    FeatureError::JobStatus(m.into())
}

pub struct TicketToPrDeps<G> {
    pub store: Arc<JobStore>,
    pub config: ConfigFn,
    pub git: G,
    pub home: PathBuf,
    pub api: Arc<dyn TicketApi>,
    pub io: Arc<dyn StartFixIo>,
    pub mover: Arc<ReviewMover>,
    pub choices: Arc<RepoChoices>,
    pub claude: Arc<dyn ClaudeRunner>,
    pub work_units: Arc<WorkUnits>,
    pub history: Option<Arc<dyn WorkspaceHistory>>,
    pub terminal: Terminal,
    pub rules: GitRemoteRules,
    /// The profile's permission mode for the Claude Code sessions this feature starts.
    pub permission_mode: String,
}

struct Inner<G> {
    d: TicketToPrDeps<G>,
    /// Jobs whose Draft with Claude run is in flight (a second click is refused).
    drafting: Mutex<HashSet<String>>,
}

pub struct TicketToPr<G>(Arc<Inner<G>>);

pub fn create_ticket_to_pr_feature<G: Git + 'static>(d: TicketToPrDeps<G>) -> Arc<TicketToPr<G>> {
    Arc::new(TicketToPr(Arc::new(Inner { d, drafting: Mutex::new(HashSet::new()) })))
}

struct Ready {
    issue_key: String,
    repo_key: String,
    dir: String,
    branch: String,
    base: String,
    summary: Option<String>,
    draft_by: Option<String>,
    pr: Value,
}

impl<G: Git + 'static> Inner<G> {
    /// The ready worktree of an awaiting-approval job, or an error for the panel. The folder is
    /// recomputed from config and must match: a job file on disk is untrusted, and this is where git
    /// pushes from and a terminal opens.
    fn ready_worktree(&self, job: &Value, config: &Value) -> FeatureResult<Ready> {
        let status = job["status"].as_str().unwrap_or("");
        if status != "awaiting-approval" {
            return Err(job_err(format!("This job is \"{status}\", not waiting for a pull request.")));
        }
        let d = &job["data"];
        let text = |k: &str| d[k].as_str().filter(|v| !v.is_empty()).map(String::from);
        let (Some(dir), Some(branch), Some(base), Some(repo_key)) = (d["ticketWorktree"]["dir"].as_str().filter(|v| !v.is_empty()), text("branch"), text("base"), text("repoKey")) else {
            return Err(job_err("This job has no worktree yet."));
        };
        let issue_key = d["issueKey"].as_str().unwrap_or("").to_string();
        let repo_path = config["repos"][&repo_key].as_str();
        // Where Start fix puts a new worktree, or a checkout of the ticket's branch that already
        // existed: the clone itself or a linked worktree git has registered for it.
        let known = repo_path.is_some_and(|rp| {
            ticket_worktree_path(Path::new(rp), &issue_key, &self.d.home).is_ok_and(|p| node_path::resolve(&p.to_string_lossy()) == node_path::resolve(dir))
                || node_path::resolve(rp) == node_path::resolve(dir)
                || is_linked_worktree_of(rp, dir)
        });
        if !known {
            return Err(job_err("This job's worktree isn't a checkout of this repository \u{2014} start again from the ticket."));
        }
        if !Path::new(dir).exists() {
            return Err(FeatureError::WorktreeGone(format!("The worktree {dir} no longer exists.")));
        }
        Ok(Ready {
            issue_key,
            repo_key,
            dir: dir.to_string(),
            branch,
            base,
            summary: d["summary"].as_str().map(String::from),
            draft_by: d["prDraft"]["by"].as_str().map(String::from),
            pr: d["pr"].clone(),
        })
    }

    /// `ready_worktree`, plus: the worktree is on the job's branch (so the commit count, the diff
    /// and the push all talk about the same branch).
    async fn checked_worktree(&self, job: &Value, config: &Value) -> FeatureResult<Ready> {
        let w = self.ready_worktree(job, config)?;
        let head = current_branch(Path::new(&w.dir), &self.d.git).await?;
        if head != w.branch {
            return Err(job_err(format!("The worktree {} is on \"{head}\", not {}. Check out {} there (no detached HEAD) and try again.", w.dir, w.branch, w.branch)));
        }
        Ok(w)
    }

    fn progress(&self, job_id: &str, step: &str, label: &str) {
        self.d.store.set_progress_if_running(job_id, step, label);
    }

    async fn run_start_fix(&self, job_id: &str, p: &super::plan::TicketToPrPayload, auth: &AuthContext) -> Result<(), String> {
        let config = (self.d.config)();
        let issue_key = p.issue_key.as_str();
        let (store, io, home) = (&self.d.store, &self.d.io, self.d.home.as_path());

        self.progress(job_id, "analysis", "Reading the saved analysis\u{2026}");
        let cached = io.read_analysis(issue_key);
        let configured = |k: &str| config["repos"][k].as_str().map(String::from);
        let mut repo_key = p.repo_key.clone().or_else(|| cached.as_ref().and_then(|c| c["repoKey"].as_str().map(String::from)));
        if repo_key.as_deref().is_none_or(|k| configured(k).is_none()) {
            let repo_keys: Vec<String> = config["repos"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
            if repo_keys.is_empty() {
                return Err("No repositories are set up yet \u{2014} add one in \u{2699} Settings, then start the fix again.".into());
            }
            // No analysis named a repository (a story, or a project Analyze doesn't cover): guess
            // from the ticket and the workspace scan, then confirm.
            self.progress(job_id, "repo", "Identifying the repository\u{2026}");
            let issue = io.issue_for_repo_guess(auth, issue_key).await?;
            let guess = choose_start_fix_repo(&issue, &config["repos"], &config["analyzeIssue"]["componentRepoMap"], &p.hint_repo_keys, self.d.rules);
            if guess.ask {
                self.progress(job_id, "repo", "Waiting for you to pick the repository\u{2026}");
                let question = RepoQuestion {
                    suggested: guess.proposal.clone(),
                    options: guess.options.iter().map(|o| RepoOption { repo_key: o.repo_key.clone(), reason: o.reason.clone() }).collect(),
                    prompt: Some(match &guess.proposal {
                        Some(pr) => format!("Start the fix for {issue_key} in {pr}? Nothing is checked out until you answer."),
                        None => format!("Which repository does {issue_key} belong to? Nothing is checked out until you answer."),
                    }),
                    no_repo_label: Some("Cancel".into()),
                    null_is_cancel: None,
                };
                repo_key = self.d.choices.ask(job_id, None, &question, "start the fix again.").await.map_err(|e| e.to_string())?;
                if repo_key.is_none() {
                    return Err("Start fix was cancelled: no repository was chosen.".into());
                }
            } else {
                repo_key = guess.proposal;
            }
            if repo_key.as_deref().is_none_or(|k| configured(k).is_none()) {
                return Err(format!("No repository could be chosen for {issue_key}."));
            }
        }
        let repo_key = repo_key.expect("a repository was chosen above");
        let repo_path = configured(&repo_key).expect("the repository is configured");
        if !Path::new(&repo_path).join(".git").exists() {
            return Err(format!("repos[\"{repo_key}\"] ({repo_path}) isn't a git clone."));
        }

        self.progress(job_id, "ticket", "Reading the ticket\u{2026}");
        let ticket = io.issue_basics(auth, issue_key).await?;
        let summary = ticket["summary"].as_str().filter(|x| !x.is_empty()).map(String::from).or_else(|| cached.as_ref().and_then(|c| c["summary"].as_str().filter(|x| !x.is_empty()).map(String::from)));
        let branch = branch_name_for(issue_key, summary.as_deref().unwrap_or(""))?;

        self.progress(job_id, "base", "Finding the default branch\u{2026}");
        let (project, repo) = split_repo_key(&repo_key);
        let mut base = match io.default_branch(auth, project, repo).await {
            Ok(b) => b.filter(|b| !b.is_empty()),
            Err(e) => {
                eprintln!("[ticket-to-pr] Bitbucket default branch lookup failed, using git's: {e}");
                None
            }
        };
        if base.as_deref().is_some_and(|b| !is_safe_branch_name(b)) {
            eprintln!("[ticket-to-pr] Bitbucket gave an unusable default branch name, using git's");
            base = None;
        }
        let base = match base {
            Some(b) => b,
            None => default_branch_from_git(Path::new(&repo_path), &self.d.git).await?,
        };
        if !is_safe_branch_name(&base) {
            return Err("The default branch name isn't one Start fix can use safely.".into());
        }

        // What exists comes first: a checkout of the ticket's branch anywhere (a worktree of yours,
        // the clone itself), then the ticket's own worktree. By now the repository is settled (an
        // analysis, a confident match, or the user's pick), so a worktree is made without asking
        // unless the setting says to ask.
        self.progress(job_id, "worktree", "Looking for an existing checkout\u{2026}");
        let repo_p = Path::new(&repo_path);
        let found = resolve_workspace(repo_p, None, Some(issue_key), &self.d.git).await?;
        let ticket_dir = ticket_worktree_path(repo_p, issue_key, home)?;
        let (wt_dir, wt_branch, wt_created, note): (String, String, bool, Option<String>) = if let Workspace::Worktree { dir, branch: b, usable, reason, .. } = &found {
            (dir.clone(), b.clone(), false, (!usable).then(|| format!("Note: {}.", reason.clone().unwrap_or_default())))
        } else if ticket_dir.exists() {
            let w = ensure_ticket_worktree(repo_p, issue_key, &branch, &base, home, &self.d.git).await?;
            (w.dir.to_string_lossy().into_owned(), w.branch, w.created, None)
        } else if p.adopt {
            return Err(format!("There is no worktree for {issue_key} to track. Use Continue in Claude Code first."));
        } else {
            let existing_branch = if let Workspace::Branch { branch: b, .. } = &found { Some(b.clone()) } else { None };
            let target = match &existing_branch {
                Some(b) => branch_worktree_path(repo_p, b, home),
                None => ticket_dir.clone(),
            };
            if config["ticketToPr"]["createWorktreeWithoutAsking"] == json!(false) {
                self.progress(job_id, "worktree", "Waiting for you to confirm a new worktree\u{2026}");
                // Worktrees are cheap to make and easy to lose track of: past a handful, say so.
                let owned = count_owned(repo_p, None, home, &self.d.git).await.unwrap_or(0);
                let crowded = if owned >= WORKTREE_SOFT_LIMIT { format!(" You already have {owned} worktrees made by the tool for this repo; \"companion worktrees --prune\" lists the ones that can go.") } else { String::new() };
                let prompt = match &existing_branch {
                    Some(b) => format!("{issue_key} has the branch {b} but no checkout of it. Create a worktree for it? Nothing is created until you answer.{crowded}"),
                    None => format!("{issue_key} has no branch or checkout yet. Create the branch {branch} from origin/{base} in a new worktree? Nothing is created until you answer.{crowded}"),
                };
                let ok = io.confirm_worktree(job_id, &WorktreeQuestion { prompt, create_label: "Create a worktree".into(), detail: target.to_string_lossy().into_owned() }).await;
                if !ok {
                    return Err("Cancelled: no worktree was created.".into());
                }
            }
            self.progress(job_id, "worktree", "Creating the worktree\u{2026}");
            match &existing_branch {
                Some(b) => {
                    let w = create_branch_worktree(repo_p, b, home, &self.d.git).await?;
                    (w.dir.to_string_lossy().into_owned(), w.branch, w.created, None)
                }
                None => {
                    let w = ensure_ticket_worktree(repo_p, issue_key, &branch, &base, home, &self.d.git).await?;
                    (w.dir.to_string_lossy().into_owned(), w.branch, w.created, None)
                }
            }
        };
        if !is_safe_branch_name(&wt_branch) {
            return Err(format!("The worktree {wt_dir} is on an unsafe branch name; check out a normal branch there and try again."));
        }
        record_work_unit(&self.d.work_units, &Unit { repo_path: &repo_path, branch: &wt_branch, dir: Some(&wt_dir), issue_key: Some(issue_key), pr_key: None });

        // An adopted worktree keeps the Start fix time the history recorded for it.
        let mut fix_started_at: Option<i64> = (!p.adopt).then(|| crate::job_files::now_ms() as i64);
        if p.adopt {
            let recorded = self.d.history.as_ref().and_then(|h| h.get_item(&format!("worktree:{wt_dir}")).ok().flatten());
            if let Some(at) = recorded.and_then(|r| r["item"]["data"]["fixStartedAt"].as_f64()) {
                fix_started_at = Some(at as i64);
            }
        }
        let mut fields = json!({ "summary": summary, "repoKey": repo_key, "repoPath": repo_path, "branch": wt_branch, "base": base, "ticketWorktree": { "dir": wt_dir } });
        if let Some(at) = fix_started_at {
            fields["fixStartedAt"] = json!(at);
        }
        data_patch(store, job_id, fields);

        let mode = self.d.permission_mode.clone();
        let mut resumed = false;
        if !p.adopt {
            // A Claude session already recorded for this branch is reopened instead of starting
            // another one beside it.
            let prior = self
                .d
                .work_units
                .find_by_issue(issue_key)
                .into_iter()
                .filter(|u| u["repoPath"].as_str().is_some_and(|r| node_path::resolve(r) == node_path::resolve(&repo_path)) && u["branch"].as_str() == Some(wt_branch.as_str()))
                .flat_map(|u| u["sessions"].as_array().cloned().unwrap_or_default())
                .rfind(|x| x["cwd"].as_str().is_some_and(|c| !c.is_empty() && node_path::resolve(c) == node_path::resolve(&wt_dir)));
            if let Some(prior) = prior {
                self.progress(job_id, "claude", "Reopening your Claude Code session\u{2026}");
                let session = json!({ "id": prior["id"], "cwd": wt_dir, "permissionMode": mode });
                let header = vec![
                    vec![HeaderSegment::styled("Ticket to PR", HeaderStyle::Title), HeaderSegment::styled(&format!("  {issue_key}"), HeaderStyle::Heading)],
                    vec![HeaderSegment::styled("Resuming your Claude Code session", HeaderStyle::Status)],
                    vec![],
                ];
                let (ok, location) = io.resume_session(&config, &session, &header, Some(crate::terminal::ticket_title(issue_key, summary.as_deref())), &wt_dir).await;
                resumed = ok;
                if resumed {
                    data_patch(store, job_id, json!({ "claudeSession": session }));
                    if let Some(location) = location {
                        data_patch(store, job_id, json!({ "terminal": location }));
                    }
                }
            }
        }

        if !p.adopt && !resumed {
            self.progress(job_id, "claude", "Opening Claude Code in plan mode\u{2026}");
            let analysis_markdown = cached.as_ref().map(|c| {
                format_analysis_as_markdown_comment(&CommentInput { issue_key: Some(issue_key), summary: c["summary"].as_str(), repo_key: c["repoKey"].as_str(), repo_match: c["repoMatch"].as_str(), analysis: &c["analysis"] })
            });
            let prompt = build_start_fix_prompt(issue_key, summary.as_deref(), &wt_branch, &base, analysis_markdown.as_deref());
            let session_id = crate::ids::uuid_v4();
            let session = json!({ "id": session_id, "cwd": wt_dir, "permissionMode": mode });
            data_patch(store, job_id, json!({ "claudeSession": session }));
            record_work_unit_session(&self.d.work_units, &Unit { repo_path: &repo_path, branch: &wt_branch, dir: Some(&wt_dir), issue_key: Some(issue_key), pr_key: None }, &Session { id: &session_id, cwd: Some(&wt_dir), feature: Some("ticket-to-pr") });
            let header = vec![
                vec![HeaderSegment::styled("Ticket to PR", HeaderStyle::Title), HeaderSegment::styled(&format!("  {issue_key}"), HeaderStyle::Heading)],
                vec![HeaderSegment::styled("Branch  ", HeaderStyle::Label), HeaderSegment::styled(&wt_branch, HeaderStyle::Branch), HeaderSegment::styled(&format!("  from origin/{base}"), HeaderStyle::Muted)],
                vec![HeaderSegment::styled("Folder  ", HeaderStyle::Label), HeaderSegment::styled(&wt_dir, HeaderStyle::Path)],
                vec![HeaderSegment::styled("Commit, push and open the pull request from here; the ticket moves to review by itself.", HeaderStyle::Status)],
                vec![],
            ];
            // Every argv word is quoted by the terminal; the prompt is one word.
            let args = ["--session-id".to_string(), session_id, "--permission-mode".to_string(), mode, prompt];
            if let Some(location) = io.open_terminal(&wt_dir, &args, &header, &TerminalOptions { title: Some(crate::terminal::ticket_title(issue_key, summary.as_deref())), ..Default::default() }).await? {
                data_patch(store, job_id, json!({ "terminal": location }));
            }
        }

        let lead = if p.adopt {
            format!("Tracking {wt_branch}: the ticket moves to review once its pull request is open.")
        } else if resumed {
            format!("Reopened your Claude Code session in {wt_dir} on {wt_branch}. Commit, push and open the pull request there.")
        } else {
            format!("{} {wt_dir} on {wt_branch} and opened Claude Code in plan mode. Commit, push and open the pull request there.", if wt_created { "Created" } else { "Reused" })
        };
        let summary = match note {
            Some(n) => format!("{lead} {n}"),
            None => lead,
        };
        patch(store, job_id, json!({ "status": "awaiting-approval", "progress": null, "result": { "summary": summary, "files": [] } }));
        Ok(())
    }

    async fn approve_inner(&self, w: &Ready, form: &(String, String), job_id: &str, auth: &AuthContext, config: &Value) -> Result<(), String> {
        let dir = Path::new(&w.dir);
        let head = current_branch(dir, &self.d.git).await?;
        if head != w.branch {
            return Err(format!("The worktree {} is on \"{head}\", not {}. Check out {} there (no detached HEAD) and try again.", w.dir, w.branch, w.branch));
        }
        // Refresh origin/<base> first: a stale ref would misreport the commits ahead.
        fetch_base(dir, &w.base, &self.d.git).await?;
        if commits_ahead(dir, &w.base, &self.d.git).await? == 0 {
            return Err(format!("{} has no commits ahead of origin/{} yet \u{2014} commit your fix first.", w.branch, w.base));
        }
        let (project, repo) = split_repo_key(&w.repo_key);
        let api = self.d.api.as_ref();
        // Reading the git host's setting first: a malformed one fails here, before anything is pushed.
        api.git_base_url()?;
        let jira_url = api.issue_url(&w.issue_key);
        let description = description_with_ticket(&form.1, &w.issue_key, &jira_url);
        patch(&self.d.store, job_id, json!({ "progress": { "stepId": "pr", "label": "Opening the pull request\u{2026}" } }));

        let input = CreatePrInput {
            issue_key: w.issue_key.clone(),
            branch: w.branch.clone(),
            base: w.base.clone(),
            title: form.0.clone(),
            description,
            transition_name: Some(config["ticketToPr"]["reviewTransitionName"].as_str().filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(default_review_transition)),
            existing_pr: safe_pr_ref(&w.pr, api, self.d.rules)?,
        };
        let steps = PrSteps { inner: self, auth, w, project, repo, form_title: &form.0 };
        let outcome = run_create_pr_steps(&input, &steps).await;

        let report = summarize_steps(&outcome.steps);
        let pr = match &outcome.pr {
            Some(p) => safe_pr_ref(&json!({ "id": p["id"], "url": p["url"], "title": if p["title"].is_null() { json!(form.0) } else { p["title"].clone() } }), api, self.d.rules)?,
            None => None,
        };
        // Data first, then the status: the history records the job as its status changes.
        let mut data = json!({
            "steps": outcome.steps.iter().map(|s| json!({ "id": s.id, "status": s.status, "detail": s.detail })).collect::<Vec<_>>(),
            "prDraft": { "title": form.0, "description": form.1, "by": w.draft_by.clone().unwrap_or_else(|| "fallback".into()) },
        });
        if let Some(pr) = &pr {
            data["pr"] = pr.clone();
        }
        data_patch(&self.d.store, job_id, data);
        if !outcome.ok {
            return Err(redact_secrets(&report)); // the caller puts the job back
        }
        let summary = match &pr {
            Some(p) => format!("Opened pull request #{} for {}.\n{report}", crate::js_text::js_to_string(&p["id"]), w.issue_key),
            None => report,
        };
        patch(&self.d.store, job_id, json!({ "status": "approved", "progress": null, "result": { "summary": summary, "files": [] } }));
        Ok(())
    }

    async fn draft_pr(&self, job: &Value) -> FeatureResult<Value> {
        let config = (self.d.config)();
        let w = self.checked_worktree(job, &config).await?;
        let dir = Path::new(&w.dir);
        // A stale origin/<base> would misreport what the branch adds.
        fetch_base(dir, &w.base, &self.d.git).await?;
        if commits_ahead(dir, &w.base, &self.d.git).await? == 0 {
            return Err(job_err(format!("{} has no commits ahead of origin/{} yet \u{2014} commit your fix first.", w.branch, w.base)));
        }
        let log = commit_log(dir, &w.base, 30, 8000, &self.d.git).await?;
        let stat = diff_stat(dir, &w.base, 4000, &self.d.git).await?;
        let summary = w.summary.clone().unwrap_or_default();
        let (title, description) = fallback_draft(&w.issue_key, Some(&summary), Some(&log), Some(&self.d.api.issue_url(&w.issue_key)));
        let mut draft = json!({ "title": title, "description": description, "by": "fallback" });
        let scratch = std::env::temp_dir().join(format!("ticket-to-pr-draft-{}", crate::ids::uuid_v4()));
        std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        let cancel = CancellationToken::new();
        let mut opts = ClaudeRunOptions::new(&scratch.to_string_lossy(), &build_draft_prompt(&w.issue_key, &summary, &log, &stat), Policy::NO_TOOLS);
        opts.cancel = Some(cancel.clone());
        match tokio::time::timeout(DRAFT_TIMEOUT, self.d.claude.run(opts)).await {
            Ok(Ok(result)) => {
                if let Some((t, d)) = parse_draft(&result.text) {
                    draft = json!({ "title": t, "description": d, "by": "claude" });
                }
            }
            Ok(Err(e)) => eprintln!("[ticket-to-pr] draft failed, using the plain draft: {e}"),
            Err(_) => {
                cancel.cancel();
                eprintln!("[ticket-to-pr] draft failed, using the plain draft: The draft took too long.");
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(self.d.store.patch_data(job["id"].as_str().unwrap_or(""), Map::from_iter([("prDraft".to_string(), draft)]))?)
    }
}

/// Create PR's steps against the real git, host and tracker.
struct PrSteps<'a, G> {
    inner: &'a Inner<G>,
    auth: &'a AuthContext,
    w: &'a Ready,
    project: &'a str,
    repo: &'a str,
    form_title: &'a str,
}

#[async_trait]
impl<G: Git + 'static> TransitionDeps for PrSteps<'_, G> {
    async fn list_transitions(&self) -> Result<Vec<Transition>, String> {
        self.inner.d.api.list_transitions(self.auth, &self.w.issue_key).await
    }
    async fn transition(&self, id: &str) -> Result<(), String> {
        self.inner.d.api.transition_issue(self.auth, &self.w.issue_key, id).await
    }
}

#[async_trait]
impl<G: Git + 'static> CreatePrDeps for PrSteps<'_, G> {
    async fn push(&self) -> Result<(), String> {
        push_branch(Path::new(&self.w.dir), &self.w.branch, &self.inner.d.git).await
    }
    async fn find_open_pr(&self) -> Result<Option<Value>, String> {
        Ok(self.inner.d.api.list_branch_pull_requests(self.auth, self.project, self.repo, &self.w.branch).await?.into_iter().find(|pr| pr["state"] == "OPEN"))
    }
    async fn repository_id(&self) -> Result<i64, String> {
        self.inner.d.api.repository_id(self.auth, self.project, self.repo).await
    }
    async fn default_reviewers(&self, repo_id: i64) -> Result<Vec<String>, String> {
        let ids = ReviewerIds { source_repo_id: repo_id, target_repo_id: repo_id, source_branch: self.w.branch.clone(), target_branch: self.w.base.clone() };
        self.inner.d.api.default_reviewers(self.auth, self.project, self.repo, &ids).await
    }
    async fn me(&self) -> Result<Option<String>, String> {
        self.inner.d.api.who_am_i(self.auth).await
    }
    async fn create_pr(&self, title: &str, description: &str, reviewers: &[String]) -> Result<Value, String> {
        let args = NewPullRequest {
            project: self.project.into(),
            repo: self.repo.into(),
            title: title.into(),
            description: description.into(),
            from_branch: self.w.branch.clone(),
            to_branch: self.w.base.clone(),
            reviewers: reviewers.to_vec(),
        };
        self.inner.d.api.create_pull_request(self.auth, &args).await
    }
    async fn add_remote_link(&self, pr: &Value) -> Result<(), String> {
        let safe = safe_pr_ref(pr, self.inner.d.api.as_ref(), self.inner.d.rules)?;
        let Some(url) = safe.as_ref().and_then(|p| p["url"].as_str()) else { return Err("Bitbucket didn't give a pull request address on the configured Bitbucket.".into()) };
        let title = pr["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(self.form_title);
        self.inner.d.api.add_remote_link(self.auth, &self.w.issue_key, url, &format!("PR #{}: {title}", crate::js_text::js_to_string(&pr["id"]))).await
    }
}

#[async_trait]
impl<G: Git + 'static> Feature for TicketToPr<G> {
    fn id(&self) -> &str {
        "ticket-to-pr"
    }

    fn label(&self) -> &str {
        "Ticket to PR"
    }

    fn start_background(&self) -> Option<Box<dyn FnOnce() + Send>> {
        Some(self.0.d.mover.start(POLL_INTERVAL))
    }

    /// A persisted job file is untrusted: the PR address the page will open is re-checked on every
    /// read.
    fn present(&self, job: Value) -> Value {
        let pr = &job["data"]["pr"];
        if !pr.is_object() {
            return job;
        }
        let mut out = job.clone();
        out["data"]["pr"] = safe_or_null(pr, self.0.d.api.as_ref(), self.0.d.rules).unwrap_or(Value::Null);
        out
    }

    async fn start(&self, payload: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let inner = &self.0;
        let config = (inner.d.config)();
        let repo_keys: Vec<String> = config["repos"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        let p = parse_ticket_to_pr_payload(&payload, &repo_keys)?;
        let job = inner.d.store.create("ticket-to-pr", Map::from_iter([("issueKey".to_string(), json!(p.issue_key)), ("adopt".to_string(), json!(p.adopt))]));
        let id = job["id"].as_str().unwrap_or("").to_string();
        let (inner, job_id) = (self.0.clone(), id);
        tokio::spawn(async move {
            if let Err(e) = inner.run_start_fix(&job_id, &p, &ctx.auth).await {
                patch(&inner.d.store, &job_id, json!({ "status": "failed", "progress": null, "error": redact_secrets(&e) }));
            }
        });
        Ok(job)
    }

    /// Create PR. Body: `{ title, description }` from the panel's form.
    async fn approve(&self, job: Value, ctx: FeatureContext) -> FeatureResult<()> {
        let inner = &self.0;
        let config = (inner.d.config)();
        let id = job["id"].as_str().unwrap_or("").to_string();
        // The job as it is now, so a second click on a stale copy is refused too; claimed before any
        // await, so a concurrent one can't race this one.
        let fresh = inner.d.store.get(&id).unwrap_or(job);
        let w = inner.ready_worktree(&fresh, &config)?;
        let form = validate_pr_form(ctx.body.as_ref().unwrap_or(&Value::Null))?;
        patch(&inner.d.store, &id, json!({ "status": "approving", "error": null, "progress": { "stepId": "check", "label": format!("Checking origin/{}\u{2026}", w.base) } }));
        // Anything below can fail (a malformed git host setting, a credential read, git, the
        // steps): the job must never be left in "approving", which nothing can leave.
        match inner.approve_inner(&w, &form, &id, &ctx.auth, &config).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = redact_secrets(&e);
                patch(&inner.d.store, &id, json!({ "status": "awaiting-approval", "progress": null, "error": msg }));
                Err(FeatureError::Other(e))
            }
        }
    }

    /// Stop tracking. The worktree and branch stay where they are.
    async fn reject(&self, job: Value, _ctx: FeatureContext) -> FeatureResult<()> {
        let status = job["status"].as_str().unwrap_or("");
        if status != "awaiting-approval" && status != "failed" {
            return Err(job_err(format!("Cannot stop tracking a job that is \"{status}\".")));
        }
        patch(&self.0.d.store, job["id"].as_str().unwrap_or(""), json!({ "status": "rejected", "progress": null }));
        Ok(())
    }

    fn action_names(&self) -> Vec<&'static str> {
        vec!["confirm-repo", "move-to-review", "draft-pr", "open-terminal"]
    }

    async fn run_action(&self, name: &str, job: Value, ctx: FeatureContext) -> FeatureResult<Value> {
        let inner = &self.0;
        match name {
            // Body { repoKey }: one of the repos offered in job.data.pendingChoice, or null to
            // cancel the fix.
            "confirm-repo" => inner.d.choices.confirm(&job, ctx.body.as_ref()),
            // Moves the ticket to its review status now (the panel's button); the poll does the same
            // by itself once a pull request from the branch is open.
            "move-to-review" => {
                if job["status"] != "awaiting-approval" {
                    return Err(job_err(format!("This job is \"{}\", not waiting for a pull request.", job["status"].as_str().unwrap_or(""))));
                }
                let out = inner.d.mover.move_now(&job, &ctx.auth).await;
                if out.status == "failed" || (out.status == "already" && job["data"]["reviewMove"].is_null()) {
                    return Err(job_err(out.detail));
                }
                Ok(inner.d.store.get(job["id"].as_str().unwrap_or("")).unwrap_or(Value::Null))
            }
            // A title and description from a Claude run with no tools, over the branch's commits;
            // falls back to a plain draft if that fails.
            "draft-pr" => {
                let id = job["id"].as_str().unwrap_or("").to_string();
                // Single flight: one Claude run per job at a time.
                if !inner.drafting.lock().unwrap().insert(id.clone()) {
                    return Err(job_err("A draft is already being written for this job \u{2014} wait for it."));
                }
                let result = inner.draft_pr(&job).await;
                inner.drafting.lock().unwrap().remove(&id);
                result
            }
            // A fresh Claude Code in the worktree (Continue in Claude Code covers the recorded
            // session; this is for an adopted worktree without one).
            "open-terminal" => {
                let w = inner.ready_worktree(&job, &(inner.d.config)())?;
                let title = job["data"]["issueKey"].as_str().map(|k| crate::terminal::ticket_title(k, job["data"]["summary"].as_str()));
                if let Some(location) = inner.d.terminal.open_claude_code(&w.dir, &[], &[], &TerminalOptions { title, ..Default::default() }).await? {
                    data_patch(&inner.d.store, job["id"].as_str().unwrap_or(""), json!({ "terminal": location.to_json() }));
                }
                Ok(json!({ "ok": true }))
            }
            other => Err(FeatureError::Other(format!("No action \"{other}\"."))),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claude::testing::FakeClaude;
    use crate::features::ticket_to_pr::review_mover::create_review_mover;
    use crate::git_cli::testing::{git as sh, make_repos, Repos};
    use crate::git_cli::SystemGit;
    use crate::paths::repo_worktrees_root;

    const BB: &str = "https://bb.example.com";

    struct FakeApi {
        base: Result<String, String>,
    }

    #[async_trait]
    impl TicketApi for FakeApi {
        fn git_base_url(&self) -> Result<String, String> {
            self.base.clone()
        }
        fn issue_url(&self, key: &str) -> String {
            format!("https://jira.example.com/browse/{key}")
        }
        async fn list_branch_pull_requests(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> Result<Vec<Value>, String> {
            Ok(vec![])
        }
        async fn repository_id(&self, _: &AuthContext, _: &str, _: &str) -> Result<i64, String> {
            Ok(1)
        }
        async fn who_am_i(&self, _: &AuthContext) -> Result<Option<String>, String> {
            Ok(Some("me".into()))
        }
        async fn default_reviewers(&self, _: &AuthContext, _: &str, _: &str, _: &ReviewerIds) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        async fn create_pull_request(&self, _: &AuthContext, _: &NewPullRequest) -> Result<Value, String> {
            Err("not expected".into())
        }
        async fn add_remote_link(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_transitions(&self, _: &AuthContext, _: &str) -> Result<Vec<Transition>, String> {
            Ok(vec![])
        }
        async fn transition_issue(&self, _: &AuthContext, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct NoMover;

    #[async_trait]
    impl MoverIo for NoMover {
        async fn find_open_pr(&self, _: &AuthContext, _: &str, _: &str, _: &str) -> Result<Option<Value>, String> {
            Ok(None)
        }
        async fn list_transitions(&self, _: &AuthContext, _: &str) -> Result<Vec<Transition>, String> {
            Ok(vec![])
        }
        async fn transition(&self, _: &AuthContext, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct FakeIo {
        analysis: Option<Value>,
        default_branch: Result<Option<String>, String>,
        confirm: Option<bool>,
        resume_ok: bool,
        terminals: Mutex<Vec<(String, Vec<String>)>>,
        asked: Mutex<Vec<WorktreeQuestion>>,
        resumes: Mutex<Vec<Value>>,
    }

    fn analysis() -> Option<Value> {
        Some(json!({ "summary": "Login fails", "repoKey": "ACME/sample-app", "repoMatch": "Bitbucket link", "analysis": { "tldr": "Missing null check" } }))
    }

    fn io(analysis: Option<Value>) -> FakeIo {
        FakeIo { analysis, default_branch: Ok(Some("master".into())), confirm: Some(true), resume_ok: false, terminals: Mutex::new(vec![]), asked: Mutex::new(vec![]), resumes: Mutex::new(vec![]) }
    }

    #[async_trait]
    impl StartFixIo for FakeIo {
        fn read_analysis(&self, _: &str) -> Option<Value> {
            self.analysis.clone()
        }
        async fn issue_basics(&self, _: &AuthContext, key: &str) -> Result<Value, String> {
            Ok(json!({ "key": key, "summary": "Login fails on Safari", "issueType": "Bug", "status": "Open" }))
        }
        async fn issue_for_repo_guess(&self, _: &AuthContext, _: &str) -> Result<Value, String> {
            Ok(json!({ "fields": { "components": [], "labels": [] } }))
        }
        async fn default_branch(&self, _: &AuthContext, _: &str, _: &str) -> Result<Option<String>, String> {
            self.default_branch.clone()
        }
        async fn open_terminal(&self, cwd: &str, args: &[String], _: &[Vec<HeaderSegment>], _: &TerminalOptions) -> Result<Option<Value>, String> {
            self.terminals.lock().unwrap().push((cwd.into(), args.to_vec()));
            Ok(Some(json!({ "app": "terminal", "windowId": "3", "tty": "/dev/ttys009" })))
        }
        async fn confirm_worktree(&self, _: &str, q: &WorktreeQuestion) -> bool {
            self.asked.lock().unwrap().push(q.clone());
            self.confirm.expect("must not ask")
        }
        async fn resume_session(&self, _: &Value, session: &Value, _: &[Vec<HeaderSegment>], _: Option<String>, _: &str) -> (bool, Option<Value>) {
            self.resumes.lock().unwrap().push(session.clone());
            (self.resume_ok, None)
        }
    }

    struct Fx {
        f: Arc<TicketToPr<SystemGit>>,
        store: Arc<JobStore>,
        io: Arc<FakeIo>,
        r: Arc<Repos>,
    }

    fn build(r: Arc<Repos>, io: FakeIo, config: Value, base: Result<String, String>) -> Fx {
        let store = Arc::new(JobStore::new());
        let io = Arc::new(io);
        let config_fn: ConfigFn = Arc::new(move || config.clone());
        let api: Arc<dyn TicketApi> = Arc::new(FakeApi { base: base.clone() });
        let mover = create_review_mover(store.clone(), config_fn.clone(), Arc::new(NoMover), Arc::new(move || base.clone()), GitRemoteRules::BitbucketDc);
        let f = create_ticket_to_pr_feature(TicketToPrDeps {
            store: store.clone(),
            config: config_fn,
            git: SystemGit,
            home: r.home.clone(),
            api,
            io: io.clone(),
            mover,
            choices: Arc::new(RepoChoices::new(store.clone())),
            claude: Arc::new(FakeClaude::new(|_| Err(crate::claude::ClaudeError::Setup("no claude in tests".into())))),
            // Shared by every feature built on these repos, as the registry file is on disk.
            work_units: Arc::new(WorkUnits::open(r.root.join("work-units.json"))),
            history: None,
            terminal: Terminal { osascript: "/no/osascript".into(), iterm_app: "/no/iterm".into(), require_macos: true, ..Terminal::default() },
            rules: GitRemoteRules::BitbucketDc,
            permission_mode: "auto".into(),
        });
        Fx { f, store, io, r }
    }

    fn config_for(r: &Repos) -> Value {
        json!({ "repos": { "ACME/sample-app": r.clone.to_string_lossy() }, "claudeModel": "m" })
    }

    fn fx(io: FakeIo) -> Fx {
        let r = Arc::new(make_repos());
        let config = config_for(&r);
        build(r, io, config, Ok(BB.into()))
    }

    /// The same, with the opt-out set: a new worktree is confirmed first.
    fn fx_asking(io: FakeIo) -> Fx {
        let r = Arc::new(make_repos());
        let mut config = config_for(&r);
        config["ticketToPr"] = json!({ "createWorktreeWithoutAsking": false });
        build(r, io, config, Ok(BB.into()))
    }

    /// Another feature (and job store) on the same clone, home and work-unit registry.
    fn again(f: &Fx, io: FakeIo, config: Value) -> Fx {
        build(f.r.clone(), io, config, Ok(BB.into()))
    }

    async fn start(f: &Fx, payload: Value) -> FeatureResult<Value> {
        f.f.start(payload, FeatureContext::default()).await
    }

    async fn settle(f: &Fx, id: &str) -> Value {
        for _ in 0..800 {
            let j = f.store.get(id).unwrap();
            if j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job never settled");
    }

    async fn run(f: &Fx, payload: Value) -> Value {
        let id = start(f, payload).await.unwrap()["id"].as_str().unwrap().to_string();
        settle(f, &id).await
    }

    async fn action(f: &Fx, name: &str, job: &Value, body: Option<Value>) -> FeatureResult<Value> {
        f.f.run_action(name, job.clone(), FeatureContext { body, ..Default::default() }).await
    }

    async fn approve(f: &Fx, job: Value) -> FeatureResult<()> {
        f.f.approve(job, FeatureContext { body: Some(json!({ "title": "t", "description": "" })), ..Default::default() }).await
    }

    fn msg<T>(r: FeatureResult<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    fn commit_fix(dir: &str, key: &str) {
        let d = Path::new(dir);
        sh(d, &["config", "user.email", "t@example.com"]);
        sh(d, &["config", "user.name", "T"]);
        std::fs::write(d.join("fix.txt"), "fix\n").unwrap();
        sh(d, &["add", "fix.txt"]);
        sh(d, &["commit", "-q", "-m", &format!("{key}: fix")]);
    }

    fn wt_dir(job: &Value) -> String {
        job["data"]["ticketWorktree"]["dir"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn start_fix_makes_the_ticket_worktree_on_a_branch_named_from_the_ticket_and_opens_claude_with_the_analysis() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "proj-7" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        let dir = repo_worktrees_root(&f.r.clone, &f.r.home).join("PROJ-7");
        assert_eq!(job["data"]["ticketWorktree"]["dir"], json!(dir));
        assert_eq!((job["data"]["branch"].clone(), job["data"]["base"].clone()), (json!("proj-7-login-fails-on-safari"), json!("master")));
        assert!(job["data"].get("worktree").is_none(), "never the deletable per-job worktree field");
        assert_eq!(sh(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "proj-7-login-fails-on-safari");
        let terminals = f.io.terminals.lock().unwrap().clone();
        assert_eq!(terminals.len(), 1);
        let args = &terminals[0].1;
        assert_eq!((args[0].as_str(), args[2].as_str(), args[3].as_str(), args.len()), ("--session-id", "--permission-mode", "auto", 5));
        assert_eq!(args[1], job["data"]["claudeSession"]["id"].as_str().unwrap());
        assert_eq!(job["data"]["claudeSession"]["cwd"], json!(dir));
        assert!(args[4].contains("Missing null check"));
        assert_eq!(terminals[0].0, dir.to_string_lossy());
        assert!(job["data"]["fixStartedAt"].is_number());
        // Where its terminal opened is kept, so the panel can bring that window to the front.
        assert_eq!(job["data"]["terminal"]["windowId"], "3");
    }

    #[tokio::test]
    async fn adopt_reuses_the_worktree_without_opening_claude_and_a_host_outage_falls_back_to_git() {
        let mut first = io(analysis());
        first.default_branch = Err("Bitbucket is down".into());
        let f = fx(first);
        let a = run(&f, json!({ "issueKey": "PROJ-7" })).await;
        assert_eq!(a["status"], "awaiting-approval", "{a}");
        assert_eq!(a["data"]["base"], "master"); // from origin/HEAD

        let g = again(&f, io(analysis()), config_for(&f.r));
        let b = run(&g, json!({ "issueKey": "PROJ-7", "adopt": true, "repoKey": "ACME/sample-app" })).await;
        assert_eq!(b["status"], "awaiting-approval", "{b}");
        assert_eq!(wt_dir(&b), wt_dir(&a));
        assert!(g.io.terminals.lock().unwrap().is_empty());
        assert!(b["data"].get("claudeSession").is_none());
    }

    #[tokio::test]
    async fn no_repository_is_a_clear_failure_and_an_unknown_repo_key_is_refused_up_front() {
        let r = Arc::new(make_repos());
        let f = build(r, io(None), json!({ "repos": {}, "claudeModel": "m" }), Ok(BB.into()));
        let c = run(&f, json!({ "issueKey": "PROJ-8" })).await;
        assert_eq!(c["status"], "failed");
        assert!(c["error"].as_str().unwrap().contains("No repositories are set up"));
        assert!(msg(start(&f, json!({ "issueKey": "PROJ-8", "repoKey": "ACME/other" })).await).contains("repositories"));
    }

    #[tokio::test]
    async fn stop_tracking_leaves_the_worktree_alone() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-9" })).await;
        f.f.reject(job.clone(), FeatureContext::default()).await.unwrap();
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "rejected");
        assert!(Path::new(&wt_dir(&job)).exists());
        assert!(msg(f.f.reject(f.store.get(job["id"].as_str().unwrap()).unwrap(), FeatureContext::default()).await).contains("Cannot stop tracking"));
    }

    #[tokio::test]
    async fn create_pr_and_the_actions_refuse_a_worktree_that_is_not_where_start_fix_puts_it_and_a_draft_needs_a_commit() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-10" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        // No commits on the branch yet: refused before any Claude run.
        assert!(msg(action(&f, "draft-pr", &job, None).await).contains("no commits ahead of origin/master"));

        // As if a job file were edited.
        f.store.patch_data(job["id"].as_str().unwrap(), json!({ "ticketWorktree": { "dir": f.r.root } }).as_object().unwrap().clone()).unwrap();
        let tampered = f.store.get(job["id"].as_str().unwrap()).unwrap();
        assert!(msg(action(&f, "open-terminal", &tampered, None).await).contains("isn't a checkout of this repository"));
        assert!(msg(approve(&f, tampered).await).contains("isn't a checkout of this repository"));
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "awaiting-approval");
    }

    #[tokio::test]
    async fn create_pr_and_draft_fetch_the_base_first_so_a_change_already_landed_there_counts_as_no_commits_ahead() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-11" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        let dir = wt_dir(&job);
        commit_fix(&dir, "PROJ-11");
        // The same commit lands on origin's master by another route (by URL, so this clone's
        // origin/master is left stale). Without a fetch it would look 1 ahead.
        sh(Path::new(&dir), &["push", "-q", &f.r.origin.to_string_lossy(), "HEAD:master"]);
        assert_eq!(sh(Path::new(&dir), &["rev-list", "--count", "refs/remotes/origin/master..HEAD"]), "1");
        assert!(msg(action(&f, "draft-pr", &job, None).await).contains("no commits ahead of origin/master"));
        assert!(msg(approve(&f, job.clone()).await).contains("no commits ahead of origin/master"));
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "awaiting-approval");
        // A malformed form is refused by the handler itself.
        let e = msg(f.f.approve(job, FeatureContext { body: None, ..Default::default() }).await);
        assert!(e.to_lowercase().contains("title"), "{e}");
    }

    #[tokio::test]
    async fn two_concurrent_create_pr_clicks_exactly_one_proceeds_and_a_failed_check_puts_the_job_back() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-12" })).await;
        let stale = f.store.get(job["id"].as_str().unwrap()).unwrap();
        let (a, b) = tokio::join!(approve(&f, stale.clone()), approve(&f, stale));
        let messages = [msg(a), msg(b)];
        assert_eq!(messages.iter().filter(|m| m.contains("no commits ahead")).count(), 1, "{messages:?}");
        assert_eq!(messages.iter().filter(|m| m.contains("\"approving\"")).count(), 1, "{messages:?}");
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "awaiting-approval");
    }

    #[tokio::test]
    async fn a_hostile_default_branch_falls_back_to_gits_and_an_existing_worktree_on_an_unsafe_branch_fails_the_job() {
        let mut hostile = io(analysis());
        hostile.default_branch = Ok(Some("--upload-pack=touch /tmp/x".into()));
        let f = fx(hostile);
        let a = run(&f, json!({ "issueKey": "PROJ-13" })).await;
        assert_eq!(a["status"], "awaiting-approval", "{a}");
        assert_eq!(a["data"]["base"], "master");

        // Someone switches the reused worktree onto a branch with an unsafe name.
        sh(Path::new(&wt_dir(&a)), &["checkout", "-q", "-b", "x;y"]);
        let g = again(&f, io(analysis()), config_for(&f.r));
        let b = run(&g, json!({ "issueKey": "PROJ-13", "adopt": true, "repoKey": "ACME/sample-app" })).await;
        assert_eq!(b["status"], "failed");
        assert!(b["error"].as_str().unwrap().contains("unsafe branch"), "{b}");
    }

    #[tokio::test]
    async fn a_worktree_on_a_different_branch_than_the_jobs_or_a_detached_head_is_refused() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-14" })).await;
        let dir = wt_dir(&job);
        sh(Path::new(&dir), &["checkout", "-q", "-b", "other-branch"]);
        assert!(msg(action(&f, "draft-pr", &job, None).await).contains("is on \"other-branch\", not"));
        sh(Path::new(&dir), &["checkout", "-q", "--detach"]);
        assert!(msg(approve(&f, job.clone()).await).contains("is on \"HEAD\", not"));
        assert_eq!(f.store.get(job["id"].as_str().unwrap()).unwrap()["status"], "awaiting-approval");
    }

    #[test]
    fn a_pull_request_address_that_is_not_a_page_on_the_configured_host_is_never_stored_or_reused() {
        let api = FakeApi { base: Ok(BB.into()) };
        let rules = GitRemoteRules::BitbucketDc;
        let good = format!("{BB}/projects/ACME/repos/sample-app/pull-requests/5");
        let url = |u: &str| safe_pr_ref(&json!({ "id": 5, "url": u }), &api, rules).unwrap().unwrap()["url"].clone();
        assert_eq!(url(&good), json!(good));
        assert_eq!(url("javascript:alert(1)"), Value::Null);
        assert_eq!(url("https://evil.example.org/projects/ACME/repos/sample-app/pull-requests/5"), Value::Null);
        assert_eq!(safe_pr_ref(&Value::Null, &api, rules).unwrap(), None);
    }

    #[tokio::test]
    async fn anything_that_fails_after_the_claim_puts_the_job_back_to_awaiting_approval_with_the_error() {
        // A host address that can't be read: the approve fails once the job is claimed.
        let r = Arc::new(make_repos());
        let config = config_for(&r);
        let f = build(r, io(analysis()), config, Err("bitbucket.baseUrl is not a string".into()));
        let job = run(&f, json!({ "issueKey": "PROJ-15" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        commit_fix(&wt_dir(&job), "PROJ-15");
        let e = msg(approve(&f, f.store.get(job["id"].as_str().unwrap()).unwrap()).await);
        assert!(e.contains("baseUrl"), "{e}");
        let after = f.store.get(job["id"].as_str().unwrap()).unwrap();
        assert_eq!(after["status"], "awaiting-approval");
        assert!(after.get("progress").is_none());
        assert!(after["error"].is_string());
    }

    #[tokio::test]
    async fn draft_with_claude_is_single_flight_per_job_and_a_served_jobs_pr_address_is_rechecked() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-16" })).await;
        // No commits: both calls fail the same way, but overlapping ones are refused earlier.
        let (a, b) = tokio::join!(action(&f, "draft-pr", &job, None), action(&f, "draft-pr", &job, None));
        let msgs = [msg(a), msg(b)];
        assert_eq!(msgs.iter().filter(|m| m.contains("already being written")).count(), 1, "{msgs:?}");
        assert_eq!(msgs.iter().filter(|m| m.contains("no commits ahead")).count(), 1, "{msgs:?}");
        // Sequential again once the first has finished.
        assert!(msg(action(&f, "draft-pr", &job, None).await).contains("no commits ahead"));

        let mut tampered = job.clone();
        tampered["data"]["pr"] = json!({ "id": 5, "url": "javascript:alert(1)" });
        assert_eq!(f.f.present(tampered.clone())["data"]["pr"]["url"], Value::Null);
        let good = format!("{BB}/projects/ACME/repos/sample-app/pull-requests/5");
        let mut served = job.clone();
        served["data"]["pr"] = json!({ "id": 5, "url": good });
        assert_eq!(f.f.present(served)["data"]["pr"]["url"], json!(good));
        assert_eq!(tampered["data"]["pr"]["url"], "javascript:alert(1)"); // not mutated
        assert_eq!(f.f.present(job.clone()), job);
    }

    async fn pending(f: &Fx, id: &str) -> Value {
        for _ in 0..400 {
            let j = f.store.get(id).unwrap();
            if !j["data"]["pendingChoice"].is_null() || j["status"] != "running" {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job never asked");
    }

    #[tokio::test]
    async fn no_analysis_start_fix_asks_which_repository_then_goes_on_with_the_pick() {
        let r = Arc::new(make_repos());
        let config = json!({ "repos": { "ACME/other": r.root.join("nope").to_string_lossy(), "ACME/sample-app": r.clone.to_string_lossy() }, "claudeModel": "m" });
        let f = build(r, io(None), config, Ok(BB.into()));
        let started = start(&f, json!({ "issueKey": "PROJ-83623", "hintRepoKeys": ["ACME/sample-app", "ACME/other", "ACME/unknown"] })).await.unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        let asking = pending(&f, &id).await;
        assert_eq!(asking["status"], "running");
        assert_eq!(asking["data"]["pendingChoice"]["suggested"], Value::Null); // two related repos: no single favourite
        let keys: Vec<&str> = asking["data"]["pendingChoice"]["options"].as_array().unwrap().iter().map(|o| o["repoKey"].as_str().unwrap()).collect();
        assert_eq!(keys, ["ACME/sample-app", "ACME/other"]);
        assert_eq!(asking["data"]["pendingChoice"]["noRepoLabel"], "Cancel");

        assert!(msg(action(&f, "confirm-repo", &asking, Some(json!({ "repoKey": "ACME/elsewhere" }))).await).contains("offered"));
        action(&f, "confirm-repo", &asking, Some(json!({ "repoKey": "ACME/sample-app" }))).await.unwrap();
        let done = settle(&f, &id).await;
        assert_eq!(done["status"], "awaiting-approval", "{done}");
        assert_eq!(done["data"]["repoKey"], "ACME/sample-app");
        assert_eq!(done["data"]["branch"], "proj-83623-login-fails-on-safari");
        assert_eq!(f.io.terminals.lock().unwrap().len(), 1);
        assert!(done["data"]["pendingChoice"].is_null());
    }

    #[tokio::test]
    async fn no_analysis_a_single_repo_from_the_workspace_scan_needs_no_question_and_cancel_fails_the_job() {
        let f = fx(io(None));
        let a = run(&f, json!({ "issueKey": "PROJ-1", "hintRepoKeys": ["ACME/sample-app"] })).await;
        assert_eq!(a["status"], "awaiting-approval", "{a}");
        assert_eq!(a["data"]["repoKey"], "ACME/sample-app");

        let g = again(&f, io(None), config_for(&f.r));
        let started = start(&g, json!({ "issueKey": "PROJ-2" })).await.unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        let asking = pending(&g, &id).await;
        assert_eq!(asking["data"]["pendingChoice"]["suggested"], Value::Null);
        action(&g, "confirm-repo", &asking, Some(json!({ "repoKey": null }))).await.unwrap();
        let b = settle(&g, &id).await;
        assert_eq!(b["status"], "failed");
        assert!(b["error"].as_str().unwrap().contains("cancelled"), "{b}");
        assert!(g.io.terminals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_checkout_of_the_tickets_branch_you_already_have_is_reused_nothing_is_created_and_nothing_is_asked() {
        let mut quiet = io(analysis());
        quiet.confirm = None; // must not ask
        let f = fx(quiet);
        let mine = f.r.root.join("my-own-tree");
        sh(&f.r.clone, &["worktree", "add", "-q", "-b", "feature/PROJ-20-login", &mine.to_string_lossy(), "origin/master"]);
        std::fs::write(mine.join("wip.txt"), "unfinished").unwrap(); // dirty is fine for your own terminal
        let job = run(&f, json!({ "issueKey": "PROJ-20" })).await;
        assert_eq!(job["status"], "awaiting-approval", "{job}");
        assert_eq!(wt_dir(&job), mine.to_string_lossy());
        assert_eq!(job["data"]["branch"], "feature/PROJ-20-login");
        assert!(f.io.asked.lock().unwrap().is_empty());
        assert!(!repo_worktrees_root(&f.r.clone, &f.r.home).exists());
        assert_eq!(f.io.terminals.lock().unwrap()[0].0, mine.to_string_lossy());
        let summary = job["result"]["summary"].as_str().unwrap();
        assert!(summary.contains("Reused") && summary.contains("on feature/PROJ-20-login") && summary.contains("uncommitted change"), "{summary}");
    }

    #[tokio::test]
    async fn a_ticket_branch_with_no_checkout_asks_first_declining_creates_nothing_accepting_makes_a_worktree_of_that_branch() {
        let mut no = io(analysis());
        no.confirm = Some(false);
        let f = fx_asking(no);
        sh(&f.r.clone, &["push", "-q", "origin", "HEAD:refs/heads/feature/PROJ-21-logout"]);
        sh(&f.r.clone, &["fetch", "-q", "origin"]);
        let declined = run(&f, json!({ "issueKey": "PROJ-21" })).await;
        assert_eq!(declined["status"], "failed");
        assert!(declined["error"].as_str().unwrap().contains("no worktree was created"));
        assert!(f.io.asked.lock().unwrap()[0].prompt.contains("feature/PROJ-21-logout but no checkout"));
        assert!(!repo_worktrees_root(&f.r.clone, &f.r.home).exists());
        assert!(f.io.terminals.lock().unwrap().is_empty());

        let g = again(&f, io(analysis()), config_for(&f.r));
        let made = run(&g, json!({ "issueKey": "PROJ-21" })).await;
        assert_eq!(made["status"], "awaiting-approval", "{made}");
        assert_eq!(made["data"]["branch"], "feature/PROJ-21-logout");
        assert_eq!(wt_dir(&made), repo_worktrees_root(&f.r.clone, &f.r.home).join("feature-PROJ-21-logout").to_string_lossy());
    }

    #[tokio::test]
    async fn a_ticket_with_no_branch_at_all_asks_before_making_one_only_when_the_setting_says_to_ask_and_otherwise_just_does_it() {
        let mut no = io(analysis());
        no.confirm = Some(false);
        let f = fx_asking(no);
        let declined = run(&f, json!({ "issueKey": "PROJ-22" })).await;
        assert_eq!(declined["status"], "failed");
        let asked = f.io.asked.lock().unwrap()[0].clone();
        assert!(asked.prompt.contains("no branch or checkout yet"));
        assert_eq!(asked.detail, repo_worktrees_root(&f.r.clone, &f.r.home).join("PROJ-22").to_string_lossy());
        assert!(!repo_worktrees_root(&f.r.clone, &f.r.home).exists());

        let mut never = io(analysis());
        never.confirm = None;
        let g = again(&f, never, config_for(&f.r));
        let made = run(&g, json!({ "issueKey": "PROJ-22" })).await;
        assert_eq!(made["status"], "awaiting-approval", "{made}");
        assert_eq!(wt_dir(&made), repo_worktrees_root(&f.r.clone, &f.r.home).join("PROJ-22").to_string_lossy());
    }

    #[tokio::test]
    async fn a_second_continue_reopens_the_recorded_claude_session_instead_of_starting_another_and_a_failed_resume_starts_a_new_one() {
        let r = Arc::new(make_repos());
        let mut cfg = config_for(&r);
        cfg["ticketToPr"] = json!({ "createWorktreeWithoutAsking": true });
        let first = build(r, io(analysis()), cfg.clone(), Ok(BB.into()));
        let job1 = run(&first, json!({ "issueKey": "PROJ-23" })).await;
        assert_eq!(job1["status"], "awaiting-approval", "{job1}");
        assert_eq!(first.io.terminals.lock().unwrap().len(), 1);

        let mut resuming = io(analysis());
        resuming.resume_ok = true;
        let second = again(&first, resuming, cfg.clone());
        let job2 = run(&second, json!({ "issueKey": "PROJ-23" })).await;
        assert_eq!(job2["status"], "awaiting-approval", "{job2}");
        assert!(second.io.terminals.lock().unwrap().is_empty());
        assert_eq!(second.io.resumes.lock().unwrap()[0]["id"], job1["data"]["claudeSession"]["id"]);
        assert_eq!(job2["data"]["claudeSession"]["id"], job1["data"]["claudeSession"]["id"]);
        assert!(job2["result"]["summary"].as_str().unwrap().contains("Reopened your Claude Code session"));

        let third = again(&first, io(analysis()), cfg);
        let job3 = run(&third, json!({ "issueKey": "PROJ-23" })).await;
        assert_eq!(third.io.terminals.lock().unwrap().len(), 1);
        assert_ne!(job3["data"]["claudeSession"]["id"], job1["data"]["claudeSession"]["id"]);
    }

    #[tokio::test]
    async fn track_pr_adopt_with_no_worktree_for_the_ticket_fails_instead_of_creating_one() {
        let mut never = io(analysis());
        never.confirm = None;
        let f = fx(never);
        let job = run(&f, json!({ "issueKey": "PROJ-24", "adopt": true })).await;
        assert_eq!(job["status"], "failed");
        assert!(job["error"].as_str().unwrap().contains("no worktree for PROJ-24 to track"), "{job}");
        assert!(!repo_worktrees_root(&f.r.clone, &f.r.home).exists());
    }

    #[tokio::test]
    async fn move_to_review_needs_a_waiting_job_and_the_action_names_are_the_four_the_panel_uses() {
        let f = fx(io(analysis()));
        let job = run(&f, json!({ "issueKey": "PROJ-25" })).await;
        assert_eq!(f.f.action_names(), ["confirm-repo", "move-to-review", "draft-pr", "open-terminal"]);
        let mut closed = job.clone();
        closed["status"] = json!("rejected");
        assert!(msg(action(&f, "move-to-review", &closed, None).await).contains("not waiting for a pull request"));
        assert!(msg(action(&f, "nope", &job, None).await).contains("No action"));
    }
}
